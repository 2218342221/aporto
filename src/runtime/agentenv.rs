//! AgentENV control plane and envd Connect/protobuf transport.
//!
//! The wire schema is pinned to AgentENV 34cdc8098096726646853a18cec7ae143995dcef.
//! Local mode still uses an AgentENV microVM server; it never runs a host shell.
mod process;
mod wire;
use process::RemoteProcess;

use crate::types::{
    CommandResult, ExecOptions, ManagedRuntime, ProcessEvent, Runtime, RuntimeProcess,
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use futures_util::StreamExt;
use prost::Message;
use reqwest::{
    Client, Response, Url,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const MAX_WIRE_FRAME: usize = 8 * 1024 * 1024;
const MAX_FILE: usize = 32 * 1024 * 1024;
const MAX_COMMAND_OUTPUT: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    Local,
    Remote,
}

#[derive(Clone)]
pub struct RuntimeConfig {
    pub mode: RuntimeMode,
    pub api_url: Option<String>,
    pub sandbox_url: Option<String>,
    pub api_key: String,
    pub template: String,
    pub timeout_ms: u64,
}

fn endpoint(value: &str, local: bool) -> Result<Url> {
    let mut url = Url::parse(value).context("invalid AgentENV endpoint")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "AgentENV requires an HTTP(S) endpoint"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "AgentENV endpoint must not contain credentials, query, or fragment"
    );
    if local {
        let host = url.host_str().unwrap_or_default();
        let host = host.trim_matches(['[', ']']);
        ensure!(
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()),
            "local runtime endpoint must be loopback"
        );
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

fn http_client(url: &Url, headers: HeaderMap) -> Result<Client> {
    let mut builder = Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .default_headers(headers);
    if url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }) {
        builder = builder.no_proxy();
    }
    Ok(builder.build()?)
}

async fn checked(response: Response) -> Result<Response> {
    let status = response.status();
    // Do not include arbitrary server error bodies: they can contain request secrets.
    ensure!(status.is_success(), "AgentENV HTTP {status}");
    Ok(response)
}

async fn bounded_body(response: Response, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .context("AgentENV response stalled")?
    {
        let chunk = chunk?;
        ensure!(
            bytes.len() + chunk.len() <= maximum,
            "AgentENV response exceeds {maximum} bytes"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct Created {
    #[serde(rename = "sandboxID")]
    id: String,
    #[serde(rename = "envdAccessToken")]
    token: Option<String>,
}

struct Inner {
    id: String,
    api: Client,
    api_url: Url,
    data: Client,
    data_url: Url,
    closed: AtomicBool,
    lifecycle: tokio::sync::Mutex<Lifecycle>,
    lease_failed: AtomicBool,
    lease: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    control_runtime: tokio::runtime::Handle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Active,
    Detached,
    Paused,
    Deleted,
}

pub struct AgentEnvRuntime {
    inner: Arc<Inner>,
}

pub async fn create_runtime(config: RuntimeConfig) -> Result<Arc<dyn Runtime>> {
    Ok(open_runtime(config, None).await?)
}

/// Create a thread workspace, or resume exactly the previously persisted sandbox.
/// A missing or unavailable sandbox is an error; it is never silently replaced.
pub async fn open_runtime(
    config: RuntimeConfig,
    sandbox_id: Option<&str>,
) -> Result<Arc<AgentEnvRuntime>> {
    ensure!(!config.api_key.is_empty(), "AgentENV API key is required");
    ensure!(!config.template.is_empty(), "AgentENV template is required");
    ensure!(
        (30_000..=86_400_000).contains(&config.timeout_ms),
        "runtime lease must be between 30 seconds and 24 hours"
    );
    let local = config.mode == RuntimeMode::Local;
    let api_url = match config.api_url.as_deref() {
        Some(value) => endpoint(value, local)?,
        None if local => endpoint("http://127.0.0.1:8000", true)?,
        None => bail!("remote runtime requires an explicit AgentENV API URL"),
    };
    let data_url = endpoint(
        config.sandbox_url.as_deref().unwrap_or(api_url.as_str()),
        local,
    )?;
    let mut headers = HeaderMap::new();
    let mut key = HeaderValue::from_str(&config.api_key).context("invalid API key")?;
    key.set_sensitive(true);
    headers.insert("X-API-Key", key);
    let api = http_client(&api_url, headers)?;
    if let Some(id) = sandbox_id {
        validate_sandbox_id(id)?;
    }
    let (path, body) = match sandbox_id {
        Some(id) => (
            format!("v2/sandboxes/{id}/connect"),
            json!({"timeout": config.timeout_ms.div_ceil(1000)}),
        ),
        None => (
            "v2/sandboxes".into(),
            json!({"templateID":config.template,"timeout":config.timeout_ms.div_ceil(1000),"autoPause":true,"network":{"allowPublicTraffic":false}}),
        ),
    };
    let response = checked(api.post(api_url.join(&path)?).timeout(Duration::from_secs(90))
        .json(&body).send().await
        .context("cannot create/connect AgentENV sandbox; local mode requires a running AgentENV server and /dev/kvm on that server")?).await?;
    let created: Created = serde_json::from_slice(&bounded_body(response, 1024 * 1024).await?)?;
    validate_sandbox_id(&created.id)?;
    ensure!(
        sandbox_id.is_none_or(|id| id == created.id),
        "AgentENV connect returned a different sandbox ID"
    );
    let data_client = (|| {
        let token = created
            .token
            .as_deref()
            .filter(|value| !value.is_empty())
            .context("AgentENV returned a sandbox without secure envd credentials")?;
        let mut headers = HeaderMap::new();
        headers.insert("x-agentenv-sandbox-id", HeaderValue::from_str(&created.id)?);
        headers.insert("x-agentenv-target-port", HeaderValue::from_static("49983"));
        let mut token = HeaderValue::from_str(token)?;
        token.set_sensitive(true);
        headers.insert("X-Access-Token", token);
        http_client(&data_url, headers)
    })();
    let data = match data_client {
        Ok(client) => client,
        Err(error) => {
            if sandbox_id.is_none() {
                let _ = api
                    .delete(api_url.join(&format!("sandboxes/{}", created.id))?)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await;
            }
            return Err(error);
        }
    };
    let inner = Arc::new(Inner {
        id: created.id,
        api,
        api_url,
        data,
        data_url,
        closed: AtomicBool::new(false),
        lifecycle: tokio::sync::Mutex::new(Lifecycle::Active),
        lease_failed: AtomicBool::new(false),
        lease: std::sync::Mutex::new(None),
        control_runtime: tokio::runtime::Handle::current(),
    });
    let weak = Arc::downgrade(&inner);
    let lease_ms = config.timeout_ms;
    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis((lease_ms / 3).min(30_000)));
        interval.tick().await;
        loop {
            interval.tick().await;
            let Some(inner) = weak.upgrade() else { break };
            if inner.closed.load(Ordering::Acquire) {
                break;
            }
            let result = inner
                .api
                .post(
                    inner
                        .api_url
                        .join(&format!("sandboxes/{}/refreshes", inner.id))
                        .expect("validated URL"),
                )
                .timeout(Duration::from_secs(10))
                .json(&json!({"duration":lease_ms.div_ceil(1000)}))
                .send()
                .await;
            inner.lease_failed.store(
                !result.is_ok_and(|r| r.status().is_success()),
                Ordering::Release,
            );
        }
    });
    *inner.lease.lock().expect("lease lock") = Some(handle);
    Ok(Arc::new(AgentEnvRuntime { inner }))
}

fn validate_sandbox_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "invalid AgentENV sandbox ID"
    );
    Ok(())
}

impl AgentEnvRuntime {
    /// Preserve the workspace for the next turn and stop paying for idle compute.
    /// Even on error, stop renewing the lease; autoPause is the fallback. The caller
    /// retains the ID and must surface the failure rather than create a replacement.
    pub async fn pause(&self) -> Result<()> {
        self.inner.finish(Lifecycle::Paused).await
    }
}

#[async_trait]
impl ManagedRuntime for AgentEnvRuntime {
    async fn pause(&self) -> Result<()> {
        AgentEnvRuntime::pause(self).await
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(handle) = self.lease.get_mut().expect("lease lock").take() {
            handle.abort();
        }
    }
}

impl Inner {
    /// Serialize teardown requests and distinguish a confirmed result from an
    /// unknown outcome. A dropped/failed request remains retryable, while data
    /// operations stay closed and the lease is no longer renewed.
    async fn finish(&self, target: Lifecycle) -> Result<()> {
        let mut lifecycle = self.lifecycle.lock().await;
        if *lifecycle == Lifecycle::Deleted || *lifecycle == target {
            return Ok(());
        }
        self.closed.store(true, Ordering::Release);
        if let Some(handle) = self.lease.lock().expect("lease lock").take() {
            handle.abort();
        }
        *lifecycle = Lifecycle::Detached;
        let path = format!("sandboxes/{}", self.id);
        let request = if target == Lifecycle::Paused {
            self.api
                .post(self.api_url.join(&format!("{path}/pause"))?)
                .timeout(Duration::from_secs(90))
        } else {
            self.api
                .delete(self.api_url.join(&path)?)
                .timeout(Duration::from_secs(30))
        };
        let response = request.send().await?;
        if target != Lifecycle::Deleted || response.status() != reqwest::StatusCode::NOT_FOUND {
            checked(response).await?;
        }
        *lifecycle = target;
        Ok(())
    }

    fn ready(&self) -> Result<()> {
        ensure!(!self.closed.load(Ordering::Acquire), "runtime is closed");
        ensure!(
            !self.lease_failed.load(Ordering::Acquire),
            "AgentENV lease refresh failed; refusing new operations until recovered"
        );
        Ok(())
    }

    async fn unary<M: Message>(&self, method: &str, message: M) -> Result<()> {
        let response = self
            .data
            .post(self.data_url.join(&format!("process.Process/{method}"))?)
            .header("Connect-Protocol-Version", "1")
            .header("Content-Type", "application/proto")
            .timeout(Duration::from_secs(10))
            .body(message.encode_to_vec())
            .send()
            .await?;
        checked(response).await?;
        Ok(())
    }
}

impl AgentEnvRuntime {
    async fn start_impl(
        &self,
        argv: &[String],
        options: ExecOptions,
        stdin: bool,
    ) -> Result<Box<dyn RuntimeProcess>> {
        self.inner.ready()?;
        ensure!(!options.cancel.is_cancelled(), "process start cancelled");
        ensure!(
            !argv.is_empty() && !argv[0].is_empty(),
            "process argv is empty"
        );
        // A persisted sandbox can outlive this host/process and its PID counter.
        let tag = format!("aporto-{}", uuid::Uuid::new_v4());
        let request = wire::StartRequest {
            process: Some(wire::ProcessConfig {
                cmd: argv[0].clone(),
                args: argv[1..].to_vec(),
                envs: options.env,
                cwd: Some(options.cwd.unwrap_or_else(|| "/workspace".into())),
            }),
            tag: Some(tag.clone()),
            stdin: Some(stdin),
        };
        let payload = request.encode_to_vec();
        let mut frame = Vec::with_capacity(payload.len() + 5);
        frame.push(0);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        let mut request = self
            .inner
            .data
            .post(self.inner.data_url.join("process.Process/Start")?)
            .header("Connect-Protocol-Version", "1")
            .header("Content-Type", "application/connect+proto")
            .body(frame);
        if let Some(timeout_ms) = options.timeout_ms {
            ensure!(timeout_ms > 0, "process timeout must be positive");
            request = request.header("Connect-Timeout-Ms", timeout_ms.to_string());
        }
        // Arm cleanup by unique tag before dispatch: envd process lifetimes are
        // independent of the HTTP request, including a dropped start request.
        let mut process = RemoteProcess {
            inner: self.inner.clone(),
            pid: 0,
            tag,
            stream: Box::pin(futures_util::stream::empty()),
            buffer: Vec::new(),
            ended: false,
            cancel: options.cancel.clone(),
        };
        let send = request.send();
        let response = tokio::select! {
            response = tokio::time::timeout(Duration::from_secs(30), send) => checked(response.context("envd start request timed out")??).await?,
            () = options.cancel.cancelled() => bail!("process start cancelled"),
        };
        process.stream = Box::pin(response.bytes_stream());
        match tokio::time::timeout(Duration::from_secs(30), process.event()).await {
            Ok(Ok(Some(wire::process_event::Event::Start(start)))) => {
                process.pid = start.pid;
                ensure!(process.pid > 0, "envd returned PID zero");
            }
            Ok(Ok(_)) => bail!("envd did not send process start event"),
            Ok(Err(error)) => return Err(error),
            Err(_) => bail!("envd process start timed out"),
        }
        Ok(Box::new(process))
    }
}

#[async_trait]
impl Runtime for AgentEnvRuntime {
    fn id(&self) -> &str {
        &self.inner.id
    }

    async fn start_process(
        &self,
        argv: &[String],
        options: ExecOptions,
    ) -> Result<Box<dyn RuntimeProcess>> {
        self.start_impl(argv, options, true).await
    }

    async fn exec(&self, command: &str, mut options: ExecOptions) -> Result<CommandResult> {
        let timeout_ms = options.timeout_ms.unwrap_or(30_000);
        ensure!(timeout_ms > 0, "command timeout must be positive");
        options.timeout_ms = Some(timeout_ms);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut process = tokio::time::timeout_at(
            deadline,
            self.start_impl(
                &["/bin/sh".into(), "-c".into(), command.into()],
                options,
                false,
            ),
        )
        .await
        .with_context(|| format!("command timed out after {timeout_ms} ms during start"))??;
        let result = tokio::time::timeout_at(deadline, async {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            while let Some(event) = process.next().await? {
                match event {
                    ProcessEvent::Stdout(data) => stdout.extend(data),
                    ProcessEvent::Stderr(data) => stderr.extend(data),
                    ProcessEvent::Exit(exit_code) => {
                        return Ok(CommandResult {
                            stdout: String::from_utf8_lossy(&stdout).into(),
                            stderr: String::from_utf8_lossy(&stderr).into(),
                            exit_code,
                        });
                    }
                }
                ensure!(
                    stdout.len() + stderr.len() <= MAX_COMMAND_OUTPUT,
                    "command output exceeds 8 MiB"
                );
            }
            bail!("envd process stream ended without an exit event")
        })
        .await;
        match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => {
                let _ = process.close().await;
                Err(error)
            }
            Err(_) => {
                let _ = process.close().await;
                bail!("command timed out after {timeout_ms} ms")
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        self.inner.ready()?;
        let response = checked(
            self.inner
                .data
                .get(self.inner.data_url.join("files")?)
                .query(&[("path", path)])
                .timeout(Duration::from_secs(60))
                .send()
                .await?,
        )
        .await?;
        bounded_body(response, MAX_FILE).await
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        self.inner.ready()?;
        ensure!(data.len() <= MAX_FILE, "file upload exceeds 32 MiB");
        let file_name = path.rsplit('/').next().unwrap_or("file").to_owned();
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(data.to_vec()).file_name(file_name),
        );
        checked(
            self.inner
                .data
                .post(self.inner.data_url.join("files")?)
                .query(&[("path", path)])
                .multipart(form)
                .timeout(Duration::from_secs(60))
                .send()
                .await?,
        )
        .await?;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.inner.finish(Lifecycle::Deleted).await
    }
}
