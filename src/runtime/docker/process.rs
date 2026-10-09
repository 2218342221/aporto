use super::{Inner, cli, inspect};
use crate::types::{ExecOptions, ProcessEvent, RuntimeProcess};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::json;
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::{Notify, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

const SUPERVISOR: &str = include_str!("supervisor.py");
const KILL: &str = include_str!("kill.py");
const MAX_FRAME: usize = 128 * 1024;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Frame {
    Started { pid: u32 },
    Stdout { data: String },
    Stderr { data: String },
    Exit { code: i32 },
    Error { message: String },
}
enum Input {
    Data(Vec<u8>, oneshot::Sender<Result<(), String>>),
    Eof(oneshot::Sender<Result<(), String>>),
}
#[derive(Default)]
struct Completion {
    result: Mutex<Option<Result<(), String>>>,
    notify: Notify,
}
impl Completion {
    async fn wait(&self) -> Result<()> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.result.lock().expect("process completion lock").clone() {
                return result.map_err(anyhow::Error::msg);
            }
            notified.await;
        }
    }
    fn finish(&self, result: Result<(), String>) {
        *self.result.lock().expect("process completion lock") = Some(result);
        self.notify.notify_waiters();
    }
}
pub(super) struct DockerProcess {
    pid: u32,
    input: mpsc::Sender<Input>,
    events: mpsc::Receiver<Result<ProcessEvent, String>>,
    stop: CancellationToken,
    completion: Arc<Completion>,
    ended: bool,
    clean_exit: bool,
}

impl DockerProcess {
    pub(super) async fn start(
        inner: Arc<Inner>,
        argv: &[String],
        options: ExecOptions,
        stdin: bool,
        maximum: usize,
    ) -> Result<Self> {
        ensure!(
            !argv.is_empty() && !argv[0].is_empty() && argv.iter().all(|arg| !arg.contains('\0')),
            "invalid guest process argv"
        );
        ensure!(
            !options.cancel.is_cancelled(),
            "guest process start cancelled"
        );
        ensure!(
            options
                .timeout_ms
                .is_none_or(|timeout| timeout > 0 && timeout <= 86_400_000),
            "guest process timeout must be between 1 ms and 24 hours"
        );
        let cwd = options.cwd.unwrap_or_else(|| "/workspace".into());
        ensure!(
            cwd.starts_with('/') && !cwd.contains('\0'),
            "guest cwd must be absolute"
        );
        ensure!(
            options.env.iter().all(|(key, value)| !key.is_empty()
                && !key.contains(['=', '\0'])
                && !value.contains('\0')),
            "invalid guest environment"
        );
        let tag = uuid::Uuid::new_v4().to_string();
        let mut payload = serde_json::to_vec(
            &json!({"tag":tag,"argv":argv,"cwd":cwd,"env":options.env,"stdin":stdin,"max_output":maximum,"timeout_ms":options.timeout_ms}),
        )?;
        ensure!(
            payload.len() < 1024 * 1024,
            "guest process start payload exceeds 1 MiB"
        );
        payload.push(b'\n');
        let mut child = cli::command(&inner.config)
            .args([
                "container",
                "exec",
                "-i",
                &inner.id,
                "python3",
                "-I",
                "-u",
                "-c",
                SUPERVISOR,
            ])
            .stdin(Stdio::piped())
            .spawn()
            .context("cannot start Docker guest supervisor")?;
        let input_pipe = child.stdin.take().context("Docker stdin unavailable")?;
        let output_pipe = child.stdout.take().context("Docker stdout unavailable")?;
        let error_pipe = child.stderr.take().context("Docker stderr unavailable")?;
        let (input_tx, input_rx) = mpsc::channel(2);
        let (event_tx, event_rx) = mpsc::channel(32);
        let (ready_tx, ready_rx) = oneshot::channel();
        let stop = CancellationToken::new();
        let completion = Arc::new(Completion::default());
        let mut process = Self {
            pid: 0,
            input: input_tx,
            events: event_rx,
            stop: stop.clone(),
            completion: completion.clone(),
            ended: false,
            clean_exit: false,
        };
        tokio::spawn(async move {
            let writer = tokio::spawn(write_input(input_pipe, input_rx, payload));
            let reader = tokio::spawn(read_output(
                output_pipe,
                event_tx.clone(),
                ready_tx,
                maximum,
            ));
            let stderr = tokio::spawn(cli::bounded(error_pipe, 64 * 1024));
            supervise(
                inner,
                tag,
                child,
                writer,
                reader,
                stderr,
                event_tx,
                stop,
                options.cancel,
                options.timeout_ms,
                completion,
            )
            .await;
        });
        match tokio::time::timeout(Duration::from_secs(30), ready_rx).await {
            Ok(Ok(Ok(pid))) => {
                process.pid = pid;
                Ok(process)
            }
            result => {
                let _ = process.close().await;
                match result {
                    Ok(Ok(Err(error))) => bail!(error),
                    _ => bail!("Docker guest process did not start"),
                }
            }
        }
    }
    pub(super) async fn close_stdin(&mut self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.input
            .send(Input::Eof(tx))
            .await
            .context("guest stdin is closed")?;
        rx.await
            .context("guest stdin writer stopped")?
            .map_err(anyhow::Error::msg)
    }
}

async fn write_input(
    mut stdin: ChildStdin,
    mut input: mpsc::Receiver<Input>,
    payload: Vec<u8>,
) -> Result<()> {
    stdin.write_all(&payload).await?;
    while let Some(message) = input.recv().await {
        match message {
            Input::Data(bytes, ack) => {
                let result = stdin
                    .write_all(&bytes)
                    .await
                    .map_err(|_| "cannot write guest stdin".to_owned());
                let failed = result.is_err();
                let _ = ack.send(result);
                if failed {
                    bail!("cannot write guest stdin");
                }
            }
            Input::Eof(ack) => {
                let result = stdin
                    .shutdown()
                    .await
                    .map_err(|_| "cannot close guest stdin".to_owned());
                let _ = ack.send(result);
                return Ok(());
            }
        }
    }
    Ok(())
}

async fn line(reader: &mut BufReader<ChildStdout>) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            ensure!(line.is_empty(), "partial Docker process frame");
            return Ok(None);
        }
        let count = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |i| i + 1);
        ensure!(
            line.len() + count <= MAX_FRAME,
            "Docker process frame exceeds limit"
        );
        line.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}
async fn read_output(
    stdout: ChildStdout,
    events: mpsc::Sender<Result<ProcessEvent, String>>,
    ready: oneshot::Sender<Result<u32, String>>,
    maximum: usize,
) -> Result<()> {
    let mut reader = BufReader::new(stdout);
    let mut ready = Some(ready);
    let result = async {
        let mut bytes = 0usize;
        while let Some(line) = line(&mut reader).await? {
            let frame: Frame =
                serde_json::from_slice(&line).context("invalid Docker supervisor frame")?;
            if let Frame::Started { pid } = frame {
                ensure!(pid > 1, "invalid guest process PID");
                let ready = ready.take().context("duplicate guest start frame")?;
                ready
                    .send(Ok(pid))
                    .map_err(|_| anyhow::anyhow!("guest process caller disappeared"))?;
                continue;
            }
            ensure!(ready.is_none(), "guest process did not send start frame");
            let event = match frame {
                Frame::Stdout { data } | Frame::Stderr { data } => {
                    let stderr = matches!(
                        serde_json::from_slice::<serde_json::Value>(&line)?["kind"].as_str(),
                        Some("stderr")
                    );
                    let data = STANDARD
                        .decode(data)
                        .context("invalid guest output encoding")?;
                    bytes = bytes
                        .checked_add(data.len())
                        .context("guest output size overflow")?;
                    ensure!(bytes <= maximum, "guest process output exceeds limit");
                    if stderr {
                        ProcessEvent::Stderr(data)
                    } else {
                        ProcessEvent::Stdout(data)
                    }
                }
                Frame::Exit { code } => {
                    events
                        .send(Ok(ProcessEvent::Exit(code)))
                        .await
                        .context("guest event receiver closed")?;
                    return Ok(());
                }
                Frame::Error { message } => {
                    let _ = message;
                    bail!("guest process failed, exceeded limits or was cancelled");
                }
                Frame::Started { .. } => unreachable!(),
            };
            events
                .send(Ok(event))
                .await
                .context("guest event receiver closed")?;
        }
        bail!("Docker process ended without an exit frame")
    }
    .await;
    if let Some(ready) = ready {
        let _ = ready.send(Err(
            "Docker guest supervisor failed before process start".into()
        ));
    }
    result
}

async fn kill_group(inner: &Inner, tag: &str) -> Result<()> {
    let container = inspect(&inner.config, &inner.image_id, &inner.id).await?;
    if container.is_none_or(|container| !container.state.running) {
        return Ok(());
    }
    let mut child = cli::command(&inner.config)
        .args([
            "container",
            "exec",
            "-i",
            &inner.id,
            "python3",
            "-I",
            "-u",
            "-c",
            KILL,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .context("cannot launch guest process cleanup")?;
    let mut stdin = child.stdin.take().context("cleanup stdin unavailable")?;
    let payload = serde_json::to_vec(&json!({"tag":tag}))?;
    let cleanup = async {
        stdin.write_all(&payload).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;
        drop(stdin);
        let stdout = child.stdout.take().context("cleanup stdout unavailable")?;
        let stderr = child.stderr.take().context("cleanup stderr unavailable")?;
        let (_, _, status) = tokio::try_join!(
            cli::bounded(stdout, 65536),
            cli::bounded(stderr, 65536),
            async { child.wait().await.context("cleanup wait failed") }
        )?;
        ensure!(status.success(), "guest process group cleanup failed");
        Ok::<_, anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(10), cleanup)
        .await
        .context("guest process group cleanup timed out")?
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    inner: Arc<Inner>,
    tag: String,
    mut child: Child,
    writer: tokio::task::JoinHandle<Result<()>>,
    mut reader: tokio::task::JoinHandle<Result<()>>,
    stderr: tokio::task::JoinHandle<Result<Vec<u8>>>,
    events: mpsc::Sender<Result<ProcessEvent, String>>,
    stop: CancellationToken,
    cancel: CancellationToken,
    timeout: Option<u64>,
    completion: Arc<Completion>,
) {
    let deadline = async {
        if let Some(ms) = timeout {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let result = tokio::select! {
        result=&mut reader=>match result {Ok(result)=>result,Err(_)=>Err(anyhow::anyhow!("Docker output supervisor stopped"))},
        ()=stop.cancelled()=>Err(anyhow::anyhow!("guest process closed")),
        ()=cancel.cancelled()=>Err(anyhow::anyhow!("guest process cancelled")),
        ()=deadline=>Err(anyhow::anyhow!("guest process timed out")),
    };
    let cleanup = if result.is_err() {
        kill_group(&inner, &tag).await
    } else {
        Ok(())
    };
    if let Err(error) = &result {
        let _ = events.try_send(Err(error.to_string()));
    }
    writer.abort();
    reader.abort();
    stderr.abort();
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    completion.finish(cleanup.map_err(|error| error.to_string()));
}

#[async_trait]
impl RuntimeProcess for DockerProcess {
    fn id(&self) -> u32 {
        self.pid
    }
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        ensure!(
            !self.ended && data.len() <= 8 * 1024 * 1024,
            "guest stdin is closed or input exceeds 8 MiB"
        );
        let (tx, rx) = oneshot::channel();
        self.input
            .send(Input::Data(data.to_vec(), tx))
            .await
            .context("guest stdin is closed")?;
        rx.await
            .context("guest stdin writer stopped")?
            .map_err(anyhow::Error::msg)
    }
    async fn next(&mut self) -> Result<Option<ProcessEvent>> {
        if self.ended {
            return Ok(None);
        }
        match self.events.recv().await {
            Some(Ok(event)) => {
                if matches!(event, ProcessEvent::Exit(_)) {
                    self.ended = true;
                    self.clean_exit = true;
                }
                Ok(Some(event))
            }
            Some(Err(error)) => {
                self.ended = true;
                Err(anyhow::Error::msg(error))
            }
            None => {
                self.ended = true;
                self.completion.wait().await?;
                bail!("Docker process event stream closed")
            }
        }
    }
    async fn close(&mut self) -> Result<()> {
        if !self.clean_exit {
            self.stop.cancel();
        }
        self.completion.wait().await
    }
}
impl Drop for DockerProcess {
    fn drop(&mut self) {
        if !self.clean_exit {
            self.stop.cancel();
        }
    }
}
