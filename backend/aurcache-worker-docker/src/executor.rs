//! The legacy container executor.
//!
//! Reproduces the pre-worker build strategy: create a container from the
//! builder image, bind a shared build directory into it, run `makepkg`, stream
//! its output as build logs, then upload whatever landed in the directory.
//!
//! This is a compatibility path. It builds in a container rather than a clean
//! chroot, so a build can see whatever the image accumulated from the previous
//! `pacman -Syu`, and it offers none of the chroot worker's caches or
//! credential handling. New deployments should use `aurcache-worker`.

use anyhow::{Context, Result, anyhow};
use aurcache_common::worker::{CompleteReport, JobDescriptor};
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::protocol::{log, remote_cancel, upload_artifacts};
use aurcache_worker_core::{artifacts, report};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, EndpointSettings, HostConfig, NetworkingConfig};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, CreateImageOptions, RemoveContainerOptions,
    StartContainerOptions,
};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::commands;
use crate::config::Config;
use crate::network::{self, NetworkPlan};

/// Where the shared build directory is mounted inside the container. The
/// server renders `PKGDEST` to this path, so makepkg writes finished packages
/// straight into the bind mount.
const CONTAINER_PKGDEST: &str = "/output";
/// Where sources are placed, inside the same bind mount.
const CONTAINER_SRC: &str = "/output/src";
/// Where the job's makepkg config is written, inside the bind mount.
const MAKEPKG_CONF_PATH: &str = "/output/makepkg.conf";
const BUILD_USER: &str = "ab";

/// Builds each package in a container spawned from the builder image.
pub struct DockerExecutor {
    cfg: Arc<Config>,
    docker: Docker,
    /// How spawned build containers are attached to the network, so the repo
    /// URL in the job's pacman.conf resolves from inside them.
    network: NetworkPlan,
}

impl DockerExecutor {
    /// Connect to the Docker daemon over the mounted socket.
    pub async fn connect(cfg: Arc<Config>) -> Result<Self> {
        let docker = Docker::connect_with_unix_defaults()?;
        docker.ping().await.map_err(|e| {
            anyhow!(
                "connection to the Docker socket failed: {e}\n\
                 The compatibility builder needs /var/run/docker.sock mounted into this \
                 container. If using podman, install 'podman-docker' to provide the docker \
                 socket, or run the chroot worker instead (see the Build Workers docs)."
            )
        })?;
        let network = network::resolve(
            |key| std::env::var(key).ok(),
            std::env::var("HOSTNAME").ok(),
        );
        tracing::info!("build containers will use network: {network:?}");
        Ok(Self {
            cfg,
            docker,
            network,
        })
    }

    /// Local and container-visible paths for one build's shared directory.
    fn job_dirs(&self, build_id: i32) -> (PathBuf, PathBuf) {
        let name = build_id.to_string();
        (
            self.cfg.dirs.local.join(&name),
            self.cfg.dirs.host.join(&name),
        )
    }

    async fn pull_image(&self, client: &WorkerClient, build_id: i32, arch: &str) -> Result<()> {
        log(
            client,
            build_id,
            &format!("[worker] pulling {}\n", self.cfg.builder_image),
        )
        .await;
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: Some(self.cfg.builder_image.clone()),
                platform: format!("linux/{}", docker_arch(arch)),
                ..Default::default()
            }),
            None,
            None,
        );
        while let Some(item) = stream.next().await {
            // A pull failure is not fatal on its own: the image may already
            // be present locally, and the build will say so far more clearly
            // than a pull error would.
            if let Err(e) = item {
                tracing::warn!("image pull reported: {e}");
                break;
            }
        }
        Ok(())
    }

    async fn build(
        &self,
        client: &Arc<WorkerClient>,
        job: &JobDescriptor,
        cancel: &AtomicBool,
    ) -> Result<CompleteReport> {
        let build_id = job.build_id;
        let (local_dir, host_dir) = self.job_dirs(build_id);

        // A crash mid-job could have left a tree behind under this id.
        let _ = std::fs::remove_dir_all(&local_dir);
        let src_dir = local_dir.join("src");
        std::fs::create_dir_all(&src_dir)
            .with_context(|| format!("creating {}", src_dir.display()))?;
        // The build container runs as its image's own unprivileged user, whose
        // uid this process cannot know and does not share. It must be able to
        // write sources, `makepkg.conf` and finished packages here, so the
        // directory is world-writable — as the pre-worker builder also made it.
        // It lives inside a per-build directory that is removed afterwards.
        world_writable(&local_dir);
        world_writable(&src_dir);

        let source = client
            .source(build_id)
            .await
            .context("downloading source")?;
        artifacts::extract_source(&source, &src_dir).context("extracting source")?;

        self.pull_image(client, build_id, &job.arch).await?;

        // Bind-mounted rather than written in: see wrap_with_config.
        let mut binds = vec![format!("{}:{CONTAINER_PKGDEST}", host_dir.display())];
        if let Some(list) = job.mirrorlist.as_deref() {
            let path = local_dir.join("mirrorlist");
            std::fs::write(&path, list).with_context(|| format!("writing {}", path.display()))?;
            binds.push(format!(
                "{}/mirrorlist:/etc/pacman.d/mirrorlist:ro",
                host_dir.display()
            ));
        }

        // The server sends no `[repo]` section; append the one rendered for the
        // host this worker reaches the server on.
        let pacman_conf = aurcache_worker_core::repo::append_to_pacman_conf(
            &job.pacman_conf,
            client.repo_section(),
        );

        let cmd = commands::wrap_with_config(
            &job.makepkg_conf,
            MAKEPKG_CONF_PATH,
            &pacman_conf,
            &commands::build_build_command(
                &job.pkgbase,
                &job.build_flags,
                Path::new(CONTAINER_SRC),
                MAKEPKG_CONF_PATH,
            ),
        );

        let container_name = format!("aurcache_build_{build_id}");
        let body = ContainerCreateBody {
            image: Some(self.cfg.builder_image.clone()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(false),
            user: Some(BUILD_USER.to_string()),
            cmd: Some(vec![
                "bash".to_string(),
                "-leco".to_string(),
                "pipefail".to_string(),
                cmd,
            ]),
            host_config: Some(HostConfig {
                auto_remove: Some(false),
                nano_cpus: self.cfg.nano_cpus(),
                memory_swap: self.cfg.memory_bytes(),
                binds: Some(binds),
                network_mode: self.network.network_mode(),
                ..Default::default()
            }),
            networking_config: self
                .network
                .endpoint_network()
                .map(|name| NetworkingConfig {
                    endpoints_config: Some(std::collections::HashMap::from([(
                        name.to_string(),
                        EndpointSettings::default(),
                    )])),
                }),
            ..Default::default()
        };

        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    ..Default::default()
                }),
                body,
            )
            .await
            .context("creating build container")?;

        let result = self
            .run_container(client, build_id, &created.id, cancel)
            .await;

        // Always remove the container, whatever happened to the build.
        let _ = self
            .docker
            .remove_container(
                &created.id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;

        let report = result?;
        if report.success {
            upload_artifacts(client, build_id, &local_dir).await?;
        }
        let _ = std::fs::remove_dir_all(&local_dir);
        Ok(report)
    }

    /// Start the container, stream its output, and wait for it while honouring
    /// cancellation and the build timeout.
    async fn run_container(
        &self,
        client: &Arc<WorkerClient>,
        build_id: i32,
        container_id: &str,
        cancel: &AtomicBool,
    ) -> Result<CompleteReport> {
        let attached = self
            .docker
            .attach_container(
                container_id,
                Some(AttachContainerOptions {
                    stdout: true,
                    stderr: true,
                    stream: true,
                    ..Default::default()
                }),
            )
            .await
            .context("attaching to build container")?;

        self.docker
            .start_container(container_id, None::<StartContainerOptions>)
            .await
            .context("starting build container")?;

        // Pump container output into the build log.
        let mut output = attached.output;
        let log_client = Arc::clone(client);
        let pump = tokio::spawn(async move {
            while let Some(Ok(chunk)) = output.next().await {
                log(&log_client, build_id, &chunk.to_string()).await;
            }
        });

        let started = Instant::now();
        let timeout = self.cfg.core.build_timeout;
        let mut wait = self.docker.wait_container(
            container_id,
            None::<bollard::query_parameters::WaitContainerOptions>,
        );
        let mut canceled = false;
        let mut timed_out = false;
        let mut exit_code: Option<i64> = None;

        loop {
            tokio::select! {
                next = wait.next() => match next {
                    Some(Ok(res)) => { exit_code = Some(res.status_code); break; }
                    // A non-zero exit surfaces here as an error carrying the code.
                    Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => {
                        exit_code = Some(code);
                        break;
                    }
                    Some(Err(e)) => return Err(e).context("waiting for build container"),
                    None => break,
                },
                () = tokio::time::sleep(Duration::from_secs(5)) => {
                    if cancel.load(Ordering::SeqCst) || remote_cancel(client, build_id).await {
                        canceled = true;
                    } else if timeout > 0 && started.elapsed().as_secs() > timeout {
                        timed_out = true;
                    } else {
                        continue;
                    }
                    let _ = self.docker.kill_container(
                        container_id,
                        None::<bollard::query_parameters::KillContainerOptions>,
                    ).await;
                    break;
                }
            }
        }

        pump.abort();

        if timed_out {
            return Ok(report::timeout_failure(started.elapsed().as_secs()));
        }
        if canceled {
            return Ok(report::classify_exit_canceled());
        }
        Ok(match exit_code {
            Some(0) => CompleteReport {
                success: true,
                exit_code: Some(0),
                reason: None,
                canceled: false,
                // The legacy container builder does not sample the build tree.
                peak_memory_bytes: None,
            },
            Some(code) => CompleteReport {
                success: false,
                exit_code: i32::try_from(code).ok(),
                reason: Some(match code {
                    137 => "build killed (OOM, exit 137)".to_string(),
                    c => format!("build failed (exit {c})"),
                }),
                canceled: false,
                // The legacy container builder does not sample the build tree.
                peak_memory_bytes: None,
            },
            None => report::setup_failure("build container exited without a status"),
        })
    }
}

impl Executor for DockerExecutor {
    const KIND: &'static str = "docker";

    async fn run_job(
        &self,
        client: Arc<WorkerClient>,
        job: JobDescriptor,
        cancel: Arc<AtomicBool>,
    ) -> CompleteReport {
        match self.build(&client, &job, &cancel).await {
            Ok(report) => report,
            Err(e) => {
                let msg = format!("build setup failed: {e:#}");
                log(&client, job.build_id, &format!("\n[worker] {msg}\n")).await;
                report::setup_failure(msg)
            }
        }
    }

    fn describe_self(&self) -> String {
        format!("legacy container ({}) [deprecated]", self.cfg.builder_image)
    }
}

/// Best-effort `chmod 0777`; a failure surfaces as a build error, not silently.
fn world_writable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)) {
            tracing::warn!("could not make {} writable: {e}", path.display());
        }
    }
}

/// Map an Arch architecture name onto Docker's platform vocabulary.
fn docker_arch(arch: &str) -> &str {
    match arch {
        "aarch64" => "arm64",
        "armv7h" => "arm/v7",
        "riscv64" => "riscv64",
        _ => "amd64",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_arch_names_to_docker_platforms() {
        assert_eq!(docker_arch("x86_64"), "amd64");
        assert_eq!(docker_arch("aarch64"), "arm64");
        assert_eq!(docker_arch("armv7h"), "arm/v7");
    }
}
