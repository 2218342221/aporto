//! Local Docker containers. Docker is an explicit operator-selected backend;
//! all application commands and files stay inside a labelled, resource-limited container.
mod cli;
mod process;

use crate::types::{
    CommandResult, ExecOptions, ManagedRuntime, ProcessEvent, Runtime, RuntimeProcess,
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
};

const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_FILE: usize = 32 * 1024 * 1024;
const MANAGED: &str = "io.aporto.managed";
const OWNER: &str = "io.aporto.owner";
const IMAGE: &str = "io.aporto.image";
const BOOT: &str = include_str!("docker/boot.py");
const READY: &str = include_str!("docker/ready.py");
const FILE_READ: &str = "import sys; data=open(sys.argv[1],'rb').read(33554433); assert len(data)<=33554432,'file exceeds limit'; sys.stdout.buffer.write(data)";
const FILE_WRITE: &str = "import sys; expected=int(sys.argv[2]); data=sys.stdin.buffer.read(expected+1); assert len(data)==expected,'incomplete file'; f=open(sys.argv[1],'wb'); f.write(data); f.close()";

#[derive(Clone, Debug)]
pub struct DockerConfig {
    pub image: String,
    pub host: String,
    pub binary: PathBuf,
    pub memory_mb: u64,
    pub cpus: u32,
    pub pids_limit: u32,
    pub network: String,
    pub owner: String,
}
impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: String::new(),
            host: "unix:///var/run/docker.sock".into(),
            binary: "docker".into(),
            memory_mb: 512,
            cpus: 2,
            pids_limit: 128,
            network: "none".into(),
            owner: String::new(),
        }
    }
}
impl DockerConfig {
    /// Resolve both the image content ID and daemon namespace before activation.
    pub async fn identity(&self) -> Result<(String, String)> {
        self.validate()?;
        let image = cli::checked(
            self,
            &["image", "inspect", "--format", "{{.Id}}", &self.image],
        )
        .await?;
        let image = std::str::from_utf8(&image)
            .context("invalid Docker image identity")?
            .trim()
            .to_owned();
        ensure!(
            image.strip_prefix("sha256:").is_some_and(valid_id),
            "invalid Docker image identity"
        );
        let daemon = cli::checked(self, &["info", "--format", "{{.ID}}"]).await?;
        let daemon = std::str::from_utf8(&daemon)
            .context("invalid Docker daemon identity")?
            .trim()
            .to_owned();
        ensure!(
            !daemon.is_empty() && daemon.len() <= 256 && !daemon.chars().any(char::is_control),
            "Docker daemon identity is unavailable"
        );
        Ok((image, daemon))
    }

    pub fn new(image: impl Into<String>, owner: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            owner: owner.into(),
            ..Self::default()
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.image.is_empty()
                && self.image.len() <= 512
                && !self.image.starts_with('-')
                && !self.image.chars().any(char::is_whitespace),
            "Docker image is required and must be a local image reference"
        );
        let socket = self
            .host
            .strip_prefix("unix://")
            .context("Docker supports only a local unix socket")?;
        ensure!(
            socket.starts_with('/')
                && !socket.contains(['?', '#', '\0'])
                && !socket.chars().any(char::is_control),
            "Docker host must be an absolute local unix socket URL"
        );
        ensure!(
            !self.binary.as_os_str().is_empty(),
            "Docker binary is required"
        );
        ensure!(
            (64..=1_048_576).contains(&self.memory_mb),
            "Docker memory must be between 64 MiB and 1 TiB"
        );
        ensure!(
            (1..=1024).contains(&self.cpus),
            "Docker CPU limit must be between 1 and 1024"
        );
        ensure!(
            (16..=65536).contains(&self.pids_limit),
            "Docker PID limit must be between 16 and 65536"
        );
        ensure!(
            matches!(self.network.as_str(), "none" | "bridge"),
            "Docker network must be none or bridge"
        );
        ensure!(
            !self.owner.is_empty()
                && self.owner.len() <= 128
                && self
                    .owner
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')),
            "Docker owner must be 1 to 128 identifier characters"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageInspection {
    id: String,
    config: ImageConfig,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageConfig {
    volumes: Option<BTreeMap<String, serde_json::Value>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspection {
    id: String,
    image: String,
    config: ContainerConfig,
    state: ContainerState,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerConfig {
    labels: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerState {
    running: bool,
    paused: bool,
}

struct Inner {
    id: String,
    config: DockerConfig,
    image_id: String,
    closed: AtomicBool,
    // 0: remove on abandonment, 1: preserve by stopping, 2: confirmed teardown,
    // 3: cleanup owned by an explicit operation; never schedule a Drop retry.
    drop_action: AtomicU8,
    lifecycle: tokio::sync::Mutex<()>,
    handle: tokio::runtime::Handle,
}
pub struct DockerRuntime {
    inner: Arc<Inner>,
}

fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}
async fn inspect(config: &DockerConfig, image: &str, id: &str) -> Result<Option<Inspection>> {
    let output = cli::run(
        config,
        &["container", "inspect", "--format", "{{json .}}", id],
    )
    .await?;
    if !output.success {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such container") || stderr.contains("No such object") {
            return Ok(None);
        }
        bail!("cannot inspect managed Docker container");
    }
    let inspection: Inspection =
        serde_json::from_slice(&output.stdout).context("invalid Docker container inspection")?;
    ensure!(
        valid_id(&inspection.id),
        "Docker returned an invalid container ID"
    );
    ensure!(
        inspection.config.labels.get(MANAGED).map(String::as_str) == Some("true")
            && inspection.config.labels.get(OWNER) == Some(&config.owner)
            && inspection.config.labels.get(IMAGE).map(String::as_str) == Some(image)
            && inspection.image == image,
        "refusing a Docker container with different owner, image or managed labels"
    );
    Ok(Some(inspection))
}
async fn remove(config: &DockerConfig, image: &str, id: &str) -> Result<()> {
    if let Some(container) = inspect(config, image, id).await? {
        cli::checked(config, &["container", "rm", "--force", &container.id]).await?;
    }
    Ok(())
}
async fn stop(config: &DockerConfig, image: &str, id: &str) -> Result<()> {
    let container = inspect(config, image, id)
        .await?
        .context("managed Docker container no longer exists")?;
    if container.state.paused {
        cli::checked(config, &["container", "unpause", &container.id]).await?;
    }
    if container.state.running {
        cli::checked(config, &["container", "stop", "--time", "1", &container.id]).await?;
    }
    Ok(())
}

impl DockerRuntime {
    /// Creation/reconnect runs independently so dropping its waiter cannot abandon
    /// an in-flight Docker create operation before its container can be cleaned up.
    pub async fn open(config: DockerConfig, sandbox_id: Option<&str>) -> Result<Arc<Self>> {
        config.validate()?;
        let sandbox_id = sandbox_id.map(str::to_owned);
        if let Some(id) = &sandbox_id {
            ensure!(valid_id(id), "invalid Docker container ID");
        }
        let created = sandbox_id.is_none();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Err(Ok(runtime)) = tx.send(Self::open_inner(config, sandbox_id).await) {
                if created {
                    let _ = runtime.close().await;
                } else {
                    let _ = runtime.pause().await;
                }
            }
        });
        rx.await.context("Docker startup supervisor stopped")?
    }

    async fn open_inner(config: DockerConfig, sandbox_id: Option<String>) -> Result<Arc<Self>> {
        let image = cli::checked(
            &config,
            &["image", "inspect", "--format", "{{json .}}", &config.image],
        )
        .await
        .context(
            "Docker image must already exist locally; images are never pulled automatically",
        )?;
        let inspection: ImageInspection =
            serde_json::from_slice(&image).context("invalid Docker image inspection")?;
        ensure!(
            inspection
                .config
                .volumes
                .as_ref()
                .is_none_or(BTreeMap::is_empty),
            "Docker images declaring VOLUME are unsupported; runtime containers must have no mounts"
        );
        let image_id = inspection.id;
        ensure!(
            image_id.strip_prefix("sha256:").is_some_and(valid_id),
            "invalid Docker image ID"
        );
        let created = sandbox_id.is_none();
        let name = sandbox_id.unwrap_or_else(|| format!("aporto-{}", uuid::Uuid::new_v4()));
        let result = async {
            let id = if created {
                let memory = format!("{}m", config.memory_mb);
                let cpus = config.cpus.to_string();
                let pids = config.pids_limit.to_string();
                let managed = format!("{MANAGED}=true");
                let owner = format!("{OWNER}={}", config.owner);
                let image = format!("{IMAGE}={image_id}");
                let image_ref = format!("io.aporto.image-ref={}", config.image);
                let output = cli::checked(
                    &config,
                    &[
                        "container",
                        "create",
                        "--pull=never",
                        "--name",
                        &name,
                        "--init",
                        "--restart=no",
                        "--network",
                        &config.network,
                        "--cap-drop=ALL",
                        "--security-opt=no-new-privileges:true",
                        "--memory",
                        &memory,
                        "--memory-swap",
                        &memory,
                        "--cpus",
                        &cpus,
                        "--pids-limit",
                        &pids,
                        "--no-healthcheck",
                        "--label",
                        &managed,
                        "--label",
                        &owner,
                        "--label",
                        &image,
                        "--label",
                        &image_ref,
                        "--workdir",
                        "/workspace",
                        "--entrypoint",
                        "python3",
                        &image_id,
                        "-I",
                        "-u",
                        "-c",
                        BOOT,
                    ],
                )
                .await?;
                let id = String::from_utf8(output)?.trim().to_owned();
                ensure!(valid_id(&id), "Docker returned an invalid container ID");
                id
            } else {
                // Reap any crash leftovers, including detached guest children.
                stop(&config, &image_id, &name).await?;
                name.clone()
            };
            cli::checked(&config, &["container", "start", &id]).await?;
            // Docker start only launches PID 1. Wait for this boot's initialization
            // before any process can create entries in the guest process registry.
            cli::checked(
                &config,
                &[
                    "container", "exec", &id, "python3", "-I", "-u", "-c", READY,
                ],
            )
            .await
            .context("Docker container initialization did not become ready")?;
            // open_inner owns cleanup until the contract check succeeds. Dropping
            // a failed startup must not race its synchronous stop/remove below.
            let inner = Arc::new(Inner {
                id,
                config: config.clone(),
                image_id: image_id.clone(),
                closed: AtomicBool::new(false),
                drop_action: AtomicU8::new(3),
                lifecycle: tokio::sync::Mutex::new(()),
                handle: tokio::runtime::Handle::current(),
            });
            let runtime = Arc::new(Self { inner });
            // Check the actual image contract before reporting a usable runtime.
            let output = runtime
                .exec(
                    "test -w /workspace && test -w /opt/agent && command -v python3 >/dev/null",
                    ExecOptions {
                        cwd: Some("/".into()),
                        timeout_ms: Some(10_000),
                        ..Default::default()
                    },
                )
                .await?;
            ensure!(
                output.exit_code == 0,
                "Docker image must provide python3, POSIX sh and writable /workspace and /opt/agent directories"
            );
            // An abandoned delivered runtime stops while preserving its workspace.
            // Undelivered creates are removed explicitly by open's supervisor.
            runtime.inner.drop_action.store(1, Ordering::Release);
            Ok::<_, anyhow::Error>(runtime)
        }
        .await;
        if result.is_err() {
            if created {
                let _ = remove(&config, &image_id, &name).await;
            } else {
                let _ = stop(&config, &image_id, &name).await;
            }
        }
        result
    }

    pub async fn pause(&self) -> Result<()> {
        self.finish(false).await
    }
    async fn finish(&self, delete: bool) -> Result<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let _lock = inner.lifecycle.lock().await;
            if inner.drop_action.load(Ordering::Acquire) == 2
                && (!delete
                    || inspect(&inner.config, &inner.image_id, &inner.id)
                        .await?
                        .is_none())
            {
                return Ok(());
            }
            inner.closed.store(true, Ordering::Release);
            inner
                .drop_action
                .store(if delete { 0 } else { 1 }, Ordering::Release);
            let result = if delete {
                remove(&inner.config, &inner.image_id, &inner.id).await
            } else {
                stop(&inner.config, &inner.image_id, &inner.id).await
            };
            // A completed failed attempt remains explicitly retryable. Do not
            // detach a second attempt after returning: another thread may now
            // reconcile and restart this same instance under its admission lock.
            inner
                .drop_action
                .store(if result.is_ok() { 2 } else { 3 }, Ordering::Release);
            result
        })
        .await
        .context("Docker lifecycle supervisor stopped")?
    }

    async fn start_impl(
        &self,
        argv: &[String],
        options: ExecOptions,
        stdin: bool,
        maximum: usize,
    ) -> Result<process::DockerProcess> {
        ensure!(
            !self.inner.closed.load(Ordering::Acquire),
            "Docker runtime is closed"
        );
        process::DockerProcess::start(self.inner.clone(), argv, options, stdin, maximum).await
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        let action = self.drop_action.load(Ordering::Acquire);
        if matches!(action, 2 | 3) {
            return;
        }
        let (config, image, id) = (self.config.clone(), self.image_id.clone(), self.id.clone());
        self.handle.spawn(async move {
            if action == 1 {
                let _ = stop(&config, &image, &id).await;
            } else {
                let _ = remove(&config, &image, &id).await;
            }
        });
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    fn id(&self) -> &str {
        &self.inner.id
    }
    async fn start_process(
        &self,
        argv: &[String],
        options: ExecOptions,
    ) -> Result<Box<dyn RuntimeProcess>> {
        Ok(Box::new(
            self.start_impl(argv, options, true, MAX_OUTPUT).await?,
        ))
    }
    async fn exec(&self, command: &str, mut options: ExecOptions) -> Result<CommandResult> {
        options.timeout_ms = Some(options.timeout_ms.unwrap_or(30_000));
        let mut process = self
            .start_impl(
                &["/bin/sh".into(), "-c".into(), command.into()],
                options,
                false,
                MAX_OUTPUT,
            )
            .await?;
        let result = collect(&mut process, MAX_OUTPUT).await;
        let cleanup = process.close().await;
        let (stdout, stderr, exit_code) = result?;
        cleanup?;
        Ok(CommandResult {
            stdout: String::from_utf8_lossy(&stdout).into(),
            stderr: String::from_utf8_lossy(&stderr).into(),
            exit_code,
        })
    }
    async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        validate_path(path)?;
        let mut process = self
            .start_impl(
                &[
                    "python3".into(),
                    "-I".into(),
                    "-c".into(),
                    FILE_READ.into(),
                    path.into(),
                ],
                ExecOptions {
                    cwd: Some("/".into()),
                    timeout_ms: Some(60_000),
                    ..Default::default()
                },
                false,
                MAX_FILE + 65536,
            )
            .await?;
        let result = collect(&mut process, MAX_FILE + 65536).await;
        let cleanup = process.close().await;
        let (stdout, _, code) = result?;
        cleanup?;
        ensure!(
            code == 0 && stdout.len() <= MAX_FILE,
            "cannot read Docker guest file or file exceeds 32 MiB"
        );
        Ok(stdout)
    }
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        validate_path(path)?;
        ensure!(data.len() <= MAX_FILE, "file upload exceeds 32 MiB");
        let mut process = self
            .start_impl(
                &[
                    "python3".into(),
                    "-I".into(),
                    "-c".into(),
                    FILE_WRITE.into(),
                    path.into(),
                    data.len().to_string(),
                ],
                ExecOptions {
                    cwd: Some("/".into()),
                    timeout_ms: Some(60_000),
                    ..Default::default()
                },
                true,
                65536,
            )
            .await?;
        let result = async {
            for chunk in data.chunks(32768) {
                process.send(chunk).await?;
            }
            process.close_stdin().await?;
            let (_, _, code) = collect(&mut process, 65536).await?;
            ensure!(code == 0, "cannot write Docker guest file");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = process.close().await;
        result?;
        cleanup
    }
    async fn close(&self) -> Result<()> {
        self.finish(true).await
    }
}
#[async_trait]
impl ManagedRuntime for DockerRuntime {
    async fn pause(&self) -> Result<()> {
        DockerRuntime::pause(self).await
    }
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/') && path.len() <= 4096 && !path.contains('\0'),
        "Docker guest file path must be absolute"
    );
    Ok(())
}
async fn collect(
    process: &mut process::DockerProcess,
    maximum: usize,
) -> Result<(Vec<u8>, Vec<u8>, i32)> {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    while let Some(event) = process.next().await? {
        match event {
            ProcessEvent::Stdout(bytes) => stdout.extend(bytes),
            ProcessEvent::Stderr(bytes) => stderr.extend(bytes),
            ProcessEvent::Exit(code) => return Ok((stdout, stderr, code)),
        }
        ensure!(
            stdout.len() + stderr.len() <= maximum,
            "Docker process output exceeds limit"
        );
    }
    bail!("Docker process ended without an exit event")
}
