//! Bounded JSON-RPC over a dedicated Core process. Never invokes a shell.
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fmt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Notify, Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct CoreProcessConfig {
    pub binary: PathBuf,
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub releases_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub max_frame_bytes: usize,
    pub max_pending: usize,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
}
impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            max_frame_bytes: aporto_protocol::MAX_FRAME_BYTES,
            max_pending: 128,
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(45),
        }
    }
}

#[derive(Clone, Debug)]
pub enum CoreError {
    Spawn(String),
    Unavailable,
    Timeout,
    Overloaded,
    Protocol(String),
    Rpc {
        code: i32,
        message: String,
        data: Option<Value>,
    },
}
impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(message) => write!(f, "cannot start Core: {message}"),
            Self::Unavailable => write!(f, "Core process is unavailable"),
            Self::Timeout => write!(f, "Core request timed out; operation completion is unknown"),
            Self::Overloaded => write!(f, "Core pending request limit reached"),
            Self::Protocol(message) => write!(f, "Core protocol error: {message}"),
            Self::Rpc { code, message, .. } => write!(f, "Core RPC error {code}: {message}"),
        }
    }
}
impl std::error::Error for CoreError {}

type Reply = Result<Value, CoreError>;
struct Inner {
    write_tx: mpsc::Sender<Vec<u8>>,
    pending: Mutex<BTreeMap<u64, oneshot::Sender<Reply>>>,
    failure: Mutex<Option<CoreError>>,
    sequence: AtomicU64,
    capacity: Semaphore,
    options: ClientOptions,
    stop: CancellationToken,
    stopped: AtomicBool,
    stopped_notification: Notify,
    shutdown_started: AtomicBool,
    shutdown_result: Mutex<Option<Result<(), CoreError>>>,
    shutdown_notification: Notify,
}
impl Inner {
    fn fail(&self, error: CoreError) {
        let mut failure = self.failure.lock().expect("Core failure lock");
        if failure.is_none() {
            *failure = Some(error.clone());
        }
        drop(failure);
        for (_, tx) in std::mem::take(&mut *self.pending.lock().expect("Core pending lock")) {
            let _ = tx.send(Err(error.clone()));
        }
        self.stop.cancel();
    }
    async fn stopped(&self) {
        loop {
            let notified = self.stopped_notification.notified();
            if self.stopped.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}
struct Owner(CancellationToken);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone)]
pub struct CoreClient {
    inner: Arc<Inner>,
    _owner: Arc<Owner>,
}
impl CoreClient {
    pub async fn spawn(
        config: CoreProcessConfig,
        options: ClientOptions,
    ) -> Result<Self, CoreError> {
        if options.max_pending == 0
            || !(256..=aporto_protocol::MAX_FRAME_BYTES).contains(&options.max_frame_bytes)
            || options.request_timeout.is_zero()
            || options.shutdown_timeout.is_zero()
        {
            return Err(CoreError::Protocol("invalid client limits".into()));
        }
        let mut command = Command::new(config.binary);
        command
            .arg("--config")
            .arg(config.config)
            .arg("--state-dir")
            .arg(config.state_dir);
        if let Some(directory) = config.releases_dir {
            command.arg("--releases-dir").arg(directory);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| CoreError::Spawn(error.to_string()))?;
        let mut stdin = child.stdin.take().expect("piped Core stdin");
        let stdout = child.stdout.take().expect("piped Core stdout");
        let stop = CancellationToken::new();
        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(options.max_pending);
        let inner = Arc::new(Inner {
            write_tx,
            pending: Mutex::new(BTreeMap::new()),
            failure: Mutex::new(None),
            sequence: AtomicU64::new(1),
            capacity: Semaphore::new(options.max_pending),
            options,
            stop: stop.clone(),
            stopped: AtomicBool::new(false),
            stopped_notification: Notify::new(),
            shutdown_started: AtomicBool::new(false),
            shutdown_result: Mutex::new(None),
            shutdown_notification: Notify::new(),
        });
        // A caller can disconnect without cancelling a partially written JSONL
        // frame. The bounded writer completes the frame independently.
        let writer_inner = inner.clone();
        tokio::spawn(async move {
            loop {
                let bytes = tokio::select! {bytes=write_rx.recv()=>match bytes {Some(bytes)=>bytes,None=>break},()=writer_inner.stop.cancelled()=>break};
                let write = async {
                    stdin.write_all(&bytes).await?;
                    stdin.flush().await
                };
                if !matches!(
                    tokio::time::timeout(writer_inner.options.request_timeout, write).await,
                    Ok(Ok(()))
                ) {
                    writer_inner.fail(CoreError::Unavailable);
                    break;
                }
            }
        });
        let reader_inner = inner.clone();
        let (reader_done_tx, reader_done_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let frame = tokio::select! {
                    frame = read_frame(&mut reader, reader_inner.options.max_frame_bytes) => frame,
                    () = reader_inner.stop.cancelled() => break,
                };
                match frame {
                    Ok(Some(frame)) => {
                        if let Err(error) = receive(&reader_inner, &frame) {
                            reader_inner.fail(error);
                            break;
                        }
                    }
                    Ok(None) => {
                        reader_inner.fail(CoreError::Unavailable);
                        break;
                    }
                    Err(error) => {
                        reader_inner.fail(error);
                        break;
                    }
                }
            }
            let _ = reader_done_tx.send(());
        });
        let manager_inner = inner.clone();
        tokio::spawn(async move {
            tokio::select! {
                _status = child.wait() => { let _ = tokio::time::timeout(Duration::from_secs(1), reader_done_rx).await; },
                () = manager_inner.stop.cancelled() => { let _ = child.kill().await; let _ = child.wait().await; },
            }
            manager_inner.fail(CoreError::Unavailable);
            manager_inner.stopped.store(true, Ordering::Release);
            manager_inner.stopped_notification.notify_waiters();
        });
        Ok(Self {
            inner,
            _owner: Arc::new(Owner(stop)),
        })
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T, CoreError> {
        self.call_with_timeout(method, params, self.inner.options.request_timeout)
            .await
    }

    async fn call_with_timeout<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        timeout: Duration,
    ) -> Result<T, CoreError> {
        if let Some(error) = self
            .inner
            .failure
            .lock()
            .expect("Core failure lock")
            .clone()
        {
            return Err(error);
        }
        let _permit = self
            .inner
            .capacity
            .try_acquire()
            .map_err(|_| CoreError::Overloaded)?;
        let id = self.inner.sequence.fetch_add(1, Ordering::Relaxed);
        if id == u64::MAX {
            self.inner.fail(CoreError::Unavailable);
            return Err(CoreError::Unavailable);
        }
        let request = aporto_protocol::RpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: serde_json::to_value(params)
                .map_err(|error| CoreError::Protocol(error.to_string()))?,
        };
        let mut bytes =
            serde_json::to_vec(&request).map_err(|error| CoreError::Protocol(error.to_string()))?;
        if bytes.len() + 1 > self.inner.options.max_frame_bytes {
            return Err(CoreError::Protocol("request frame too large".into()));
        }
        bytes.push(b'\n');
        let (tx, rx) = oneshot::channel();
        // Pair failure inspection and registration under the same lock. Otherwise
        // a process exit can drain pending after the first check but before this
        // insertion, leaving the new caller stranded until its request timeout.
        {
            let failure = self.inner.failure.lock().expect("Core failure lock");
            if let Some(error) = failure.as_ref() {
                return Err(error.clone());
            }
            self.inner
                .pending
                .lock()
                .expect("Core pending lock")
                .insert(id, tx);
        }
        let _guard = PendingGuard {
            id,
            inner: Arc::downgrade(&self.inner),
        };
        self.inner
            .write_tx
            .try_send(bytes)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => CoreError::Overloaded,
                mpsc::error::TrySendError::Closed(_) => CoreError::Unavailable,
            })?;
        // Never retry a timed out request: turn/start can already have committed.
        let result = tokio::time::timeout(timeout, async {
            rx.await.map_err(|_| CoreError::Unavailable)?
        })
        .await
        .map_err(|_| CoreError::Timeout)??;
        serde_json::from_value(result)
            .map_err(|error| CoreError::Protocol(format!("invalid result: {error}")))
    }

    pub fn is_available(&self) -> bool {
        !self.inner.stop.is_cancelled()
            && self
                .inner
                .failure
                .lock()
                .expect("Core failure lock")
                .is_none()
    }

    /// Wait until the Core child has actually exited and been reaped. Cancelling
    /// this waiter does not stop the independent process manager or other waiters.
    pub async fn wait_closed(&self) {
        self.inner.stopped().await;
    }

    /// One cleanup supervisor serves every waiter and survives caller cancellation.
    /// A confirmed shutdown RPC failure is preserved after the process is reaped.
    pub async fn shutdown(&self) -> Result<(), CoreError> {
        if !self.inner.shutdown_started.swap(true, Ordering::AcqRel) {
            let client = self.clone();
            tokio::spawn(async move {
                let result = client.shutdown_inner().await;
                *client
                    .inner
                    .shutdown_result
                    .lock()
                    .expect("Core shutdown lock") = Some(result);
                client.inner.shutdown_notification.notify_waiters();
            });
        }
        loop {
            let notified = self.inner.shutdown_notification.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .inner
                .shutdown_result
                .lock()
                .expect("Core shutdown lock")
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }

    async fn shutdown_inner(&self) -> Result<(), CoreError> {
        if self.inner.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let grace = self.inner.options.shutdown_timeout;
        let deadline = tokio::time::Instant::now() + grace;
        // Cleanup of an already failed process remains idempotent; an error from
        // an explicitly requested graceful shutdown must not be treated as success.
        let result = if self.is_available() {
            tokio::time::timeout_at(
                deadline,
                self.call_with_timeout::<Value>("shutdown", json!({}), grace),
            )
            .await
            .map_err(|_| CoreError::Timeout)
            .and_then(|result| result)
            .map(|_| ())
        } else {
            self.inner.stop.cancel();
            Ok(())
        };
        if tokio::time::timeout_at(deadline, self.inner.stopped())
            .await
            .is_err()
        {
            self.inner.stop.cancel();
            // Reaping after SIGKILL is separate from the graceful cleanup window.
            tokio::time::timeout(Duration::from_secs(5), self.inner.stopped())
                .await
                .map_err(|_| CoreError::Timeout)?;
            return result.and(Err(CoreError::Timeout));
        }
        result
    }
}

struct PendingGuard {
    id: u64,
    inner: Weak<Inner>,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .pending
                .lock()
                .expect("Core pending lock")
                .remove(&self.id);
        }
    }
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    maximum: usize,
) -> Result<Option<Vec<u8>>, CoreError> {
    let mut frame = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|_| CoreError::Unavailable)?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(CoreError::Protocol("truncated JSONL frame".into()))
            };
        }
        let end = available.iter().position(|&byte| byte == b'\n');
        let count = end.map_or(available.len(), |end| end + 1);
        if frame.len() + count > maximum {
            return Err(CoreError::Protocol("response frame too large".into()));
        }
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            frame.pop();
            return Ok(Some(frame));
        }
    }
}

fn receive(inner: &Inner, frame: &[u8]) -> Result<(), CoreError> {
    let value: Value = serde_json::from_slice(frame)
        .map_err(|_| CoreError::Protocol("invalid JSON response".into()))?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(CoreError::Protocol("invalid JSON-RPC version".into()));
    }
    // Notifications carry no request state; events are fetched using event/list.
    if value.get("method").is_some() && value.get("id").is_none() {
        return Ok(());
    }
    let id = value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| CoreError::Protocol("response ID must be u64".into()))?;
    let result = match (value.get("result"), value.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => Err(CoreError::Rpc {
            code: error
                .get("code")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .ok_or_else(|| CoreError::Protocol("invalid RPC error code".into()))?,
            message: error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| CoreError::Protocol("invalid RPC error message".into()))?
                .to_owned(),
            data: error.get("data").cloned(),
        }),
        _ => {
            return Err(CoreError::Protocol(
                "response must have exactly one result or error".into(),
            ));
        }
    };
    // A late reply after timeout is expected and is discarded without retry.
    if let Some(tx) = inner.pending.lock().expect("Core pending lock").remove(&id) {
        let _ = tx.send(result);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failure_during_serialization_cannot_register_an_abandoned_waiter() {
        let stop = CancellationToken::new();
        let (write_tx, mut write_rx) = mpsc::channel(1);
        let inner = Arc::new(Inner {
            write_tx,
            pending: Mutex::new(BTreeMap::new()),
            failure: Mutex::new(None),
            sequence: AtomicU64::new(1),
            capacity: Semaphore::new(1),
            options: ClientOptions {
                request_timeout: Duration::from_millis(10),
                ..ClientOptions::default()
            },
            stop: stop.clone(),
            stopped: AtomicBool::new(false),
            stopped_notification: Notify::new(),
            shutdown_started: AtomicBool::new(false),
            shutdown_result: Mutex::new(None),
            shutdown_notification: Notify::new(),
        });
        struct FailDuringSerialization(Arc<Inner>);
        impl Serialize for FailDuringSerialization {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.0.fail(CoreError::Unavailable);
                serializer.serialize_unit()
            }
        }
        let client = CoreClient {
            inner: inner.clone(),
            _owner: Arc::new(Owner(stop)),
        };
        let result = client
            .call::<Value>("echo", FailDuringSerialization(inner.clone()))
            .await;
        assert!(matches!(result, Err(CoreError::Unavailable)));
        assert!(inner.pending.lock().unwrap().is_empty());
        assert!(
            write_rx.try_recv().is_err(),
            "a dead process must not receive a newly registered request"
        );
    }
}
