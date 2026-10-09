//! Durable application service. Transports never own agent execution or thread state.
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use aporto_protocol::*;
use async_trait::async_trait;
use futures_util::FutureExt;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Notify, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub mod executor;
mod store;
use store::Store;

#[derive(Clone, Debug)]
pub struct CoreLimits {
    pub max_concurrent: usize,
    pub max_queued: usize,
    pub max_threads: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_history_bytes: usize,
    pub max_event_bytes: usize,
    pub max_events_per_turn: usize,
    pub shutdown_grace: Duration,
}

impl Default for CoreLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            max_queued: 64,
            max_threads: 1000,
            max_input_bytes: 32 * 1024,
            max_output_bytes: 32 * 1024,
            max_history_bytes: 8 * 1024 * 1024,
            max_event_bytes: 256 * 1024,
            max_events_per_turn: 2000,
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

pub struct ExecutionContext {
    pub thread_id: String,
    pub turn_id: String,
    pub agent_id: String,
    pub bundle_digest: String,
    pub release_id: String,
    pub sandbox_id: Option<String>,
    pub runtime_instance_id: String,
    pub runtime_image: String,
    pub runtime_provider: String,
    pub workdir: String,
    pub input: String,
    pub history: Vec<Value>,
    pub recovery_note: Option<String>,
    pub cancel: CancellationToken,
    pub events: EventSink,
}

#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub answer: String,
    pub history: Vec<Value>,
}

#[async_trait]
pub trait TurnExecutor: Send + Sync {
    fn agents(&self) -> Vec<AgentSummary>;
    fn runtime_options(
        &self,
        agent_id: &str,
        release_id: &str,
        bundle_digest: &str,
    ) -> Option<RuntimeOptions> {
        self.agents()
            .into_iter()
            .find(|agent| {
                agent.id == agent_id
                    && agent.release_id == release_id
                    && agent.bundle_digest == bundle_digest
            })
            .map(|agent| agent.runtime)
    }
    fn supports_release(&self, agent_id: &str, release_id: &str, bundle_digest: &str) -> bool {
        self.runtime_options(agent_id, release_id, bundle_digest)
            .is_some()
    }
    async fn execute(&self, context: ExecutionContext) -> Result<ExecutionResult>;
}

#[derive(Clone)]
pub struct EventSink {
    store: Weak<Store>,
    thread_id: String,
    turn_id: String,
}

impl EventSink {
    pub async fn emit(&self, kind: &str, data: Value) -> Result<()> {
        let store = self.store.upgrade().context("Core event store is closed")?;
        let thread_id = self.thread_id.clone();
        let turn_id = self.turn_id.clone();
        let kind = kind.to_string();
        tokio::task::spawn_blocking(move || store.emit(&thread_id, &turn_id, &kind, data)).await?
    }
}

#[derive(Clone)]
struct Job {
    thread_id: String,
    turn_id: String,
    runtime_instance_id: String,
    cancel: CancellationToken,
}

struct Scheduling {
    queue: VecDeque<Job>,
    running: BTreeMap<String, Job>,
    interrupt_reasons: BTreeMap<String, String>,
    stopping: bool,
}

struct Inner {
    store: Arc<Store>,
    executor: Arc<dyn TurnExecutor>,
    agents: BTreeMap<String, AgentSummary>,
    limits: CoreLimits,
    scheduling: Mutex<Scheduling>,
    workers: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
    notify: Notify,
    initialized: AtomicBool,
    rpc_slots: Arc<Semaphore>,
    shutdown_started: AtomicBool,
    shutdown_result: Mutex<Option<std::result::Result<(), String>>>,
    shutdown_done: Notify,
    failure: CancellationToken,
}

#[derive(Clone)]
pub struct CoreService {
    inner: Arc<Inner>,
}

fn error(code: i32, message: &str) -> anyhow::Error {
    RpcError::new(code, message).into()
}

fn params<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| error(INVALID_PARAMS, "invalid method parameters"))
}

pub(crate) fn normalize_runtime_options(mut options: RuntimeOptions) -> Result<RuntimeOptions> {
    ensure!(
        !options.provider.is_empty()
            && options.provider.len() <= 64
            && options
                .provider
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
        "invalid runtime provider"
    );
    if !options.images.contains(&options.default_image) {
        options.images.push(options.default_image.clone());
    }
    let mut images = Vec::new();
    for image in options.images {
        ensure!(
            !image.is_empty() && image.len() <= 1024 && !image.chars().any(char::is_control),
            "invalid runtime image"
        );
        if !images.contains(&image) {
            images.push(image);
        }
    }
    ensure!(
        !images.is_empty() && images.len() <= 100,
        "runtime requires 1..100 configured images"
    );
    options.images = images;
    options.default_workdir = aporto::workspace::validate_workdir(&options.default_workdir)?;
    Ok(options)
}

impl CoreService {
    pub async fn open(
        state_dir: &Path,
        executor: Arc<dyn TurnExecutor>,
        limits: CoreLimits,
    ) -> Result<Self> {
        ensure!(
            (1..=64).contains(&limits.max_concurrent) && (1..=10_000).contains(&limits.max_queued),
            "invalid worker or queue limit"
        );
        ensure!(
            limits.max_threads > 0
                && limits.max_input_bytes > 0
                && limits.max_input_bytes <= 32 * 1024
                && limits.max_output_bytes > 0
                && limits.max_output_bytes <= 32 * 1024
                && limits.max_history_bytes > 0
                && limits.max_history_bytes <= 64 * 1024 * 1024
                && limits.max_event_bytes > 0
                && limits.max_event_bytes <= 256 * 1024
                && limits.max_events_per_turn >= 16
                && limits.shutdown_grace <= Duration::from_secs(300),
            "invalid Core resource limit"
        );
        let mut agents = BTreeMap::new();
        for mut agent in executor.agents() {
            ensure!(
                !agent.id.is_empty()
                    && agent.id.len() <= 128
                    && !agent.id.chars().any(char::is_control),
                "invalid configured agent id"
            );
            agent.runtime = normalize_runtime_options(agent.runtime)?;
            ensure!(
                agents.insert(agent.id.clone(), agent).is_none(),
                "duplicate configured agent id"
            );
        }
        ensure!(
            !agents.is_empty() && agents.len() <= 1000,
            "Core requires 1..1000 configured agents"
        );
        let directory = state_dir.to_path_buf();
        let store_limits = limits.clone();
        let recovery_executor = executor.clone();
        let store = Arc::new(
            tokio::task::spawn_blocking(move || -> Result<Store> {
                let store = Store::open(&directory, store_limits)?;
                store.bind_legacy_instances(recovery_executor.as_ref())?;
                Ok(store)
            })
            .await??,
        );
        let inner = Arc::new(Inner {
            store,
            executor,
            agents,
            limits: limits.clone(),
            scheduling: Mutex::new(Scheduling {
                queue: VecDeque::new(),
                running: BTreeMap::new(),
                interrupt_reasons: BTreeMap::new(),
                stopping: false,
            }),
            workers: tokio::sync::Mutex::new(Vec::new()),
            notify: Notify::new(),
            initialized: AtomicBool::new(false),
            rpc_slots: Arc::new(Semaphore::new(32)),
            shutdown_started: AtomicBool::new(false),
            shutdown_result: Mutex::new(None),
            shutdown_done: Notify::new(),
            failure: CancellationToken::new(),
        });
        for _ in 0..limits.max_concurrent {
            inner
                .workers
                .lock()
                .await
                .push(tokio::spawn(worker(inner.clone())));
        }
        Ok(Self { inner })
    }

    /// Requests are short transactions; turn execution continues in bounded background workers.
    pub async fn handle(&self, request: RpcRequest) -> RpcResponse {
        let id = request.id;
        if request.method == "shutdown" {
            if request.jsonrpc != "2.0" {
                return RpcResponse::failure(
                    Some(id),
                    RpcError::new(INVALID_REQUEST, "jsonrpc must be 2.0"),
                );
            }
            if !self.inner.initialized.load(Ordering::Acquire) {
                return RpcResponse::failure(
                    Some(id),
                    RpcError::new(
                        UNAVAILABLE,
                        "initialize must complete before other requests",
                    ),
                );
            }
            if !request.params.is_null()
                && !request
                    .params
                    .as_object()
                    .is_some_and(|object| object.is_empty())
            {
                return RpcResponse::failure(
                    Some(id),
                    RpcError::new(INVALID_PARAMS, "shutdown takes no parameters"),
                );
            }
            return match self.shutdown().await {
                Ok(()) => RpcResponse::success(id, json!({"status":"stopped"})),
                Err(_) => RpcResponse::failure(
                    Some(id),
                    RpcError::new(INTERNAL_ERROR, "Core shutdown persistence failed"),
                ),
            };
        }
        let permit = match self.inner.rpc_slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return RpcResponse::failure(
                    Some(id),
                    RpcError::new(OVERLOADED, "too many in-flight Core requests"),
                );
            }
        };
        let this = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            this.dispatch(request)
        })
        .await;
        match result {
            Ok(Ok(value)) => {
                if serde_json::to_vec(&value)
                    .is_ok_and(|bytes| bytes.len() <= MAX_FRAME_BYTES - 256)
                {
                    RpcResponse::success(id, value)
                } else {
                    RpcResponse::failure(
                        Some(id),
                        RpcError::new(
                            OVERLOADED,
                            "response exceeds frame limit; request a smaller page",
                        ),
                    )
                }
            }
            Ok(Err(error)) => {
                let error = error
                    .downcast_ref::<RpcError>()
                    .cloned()
                    .unwrap_or_else(|| {
                        self.inner.failure.cancel();
                        RpcError::new(INTERNAL_ERROR, "Core operation failed")
                    });
                RpcResponse::failure(Some(id), error)
            }
            Err(_) => {
                self.inner.failure.cancel();
                RpcResponse::failure(
                    Some(id),
                    RpcError::new(INTERNAL_ERROR, "Core operation failed"),
                )
            }
        }
    }

    fn dispatch(&self, request: RpcRequest) -> Result<Value> {
        if request.jsonrpc != "2.0" {
            return Err(error(INVALID_REQUEST, "jsonrpc must be 2.0"));
        }
        if request.method == "initialize" {
            let input: InitializeParams = params(request.params)?;
            if input.protocol_version != PROTOCOL_VERSION {
                return Err(error(CONFLICT, "unsupported protocol version"));
            }
            if input.client_name.is_empty() || input.client_name.len() > 128 {
                return Err(error(INVALID_PARAMS, "invalid client_name"));
            }
            self.inner.initialized.store(true, Ordering::Release);
            return Ok(serde_json::to_value(InitializeResult {
                protocol_version: PROTOCOL_VERSION.into(),
                server_name: "aporto-core".into(),
                capabilities: vec![
                    "durable_threads",
                    "idempotent_turns",
                    "event_replay",
                    "item/list",
                    "runtime/instances",
                    "shared_runtime_instances",
                    "interrupt",
                    "bounded_queue",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            })?);
        }
        if !self.inner.initialized.load(Ordering::Acquire) {
            return Err(error(
                UNAVAILABLE,
                "initialize must complete before other requests",
            ));
        }
        match request.method.as_str() {
            "agent/list" => Ok(serde_json::to_value(AgentListResult {
                agents: self.inner.agents.values().cloned().collect(),
            })?),
            "thread/start" => {
                let input: ThreadStartParams = params(request.params)?;
                let agent = self
                    .inner
                    .agents
                    .get(&input.agent_id)
                    .ok_or_else(|| error(NOT_FOUND, "configured agent not found"))?;
                let title = input.title.unwrap_or_else(|| "New chat".into());
                if title.trim().is_empty() || title.len() > 200 {
                    return Err(error(INVALID_PARAMS, "title must contain 1..200 bytes"));
                }
                let state = self.inner.scheduling.lock().unwrap();
                if state.stopping || self.is_shutting_down() {
                    return Err(error(UNAVAILABLE, "Core is shutting down"));
                }
                Ok(serde_json::to_value(
                    self.inner.store.start_thread_selected(
                        agent,
                        &title,
                        input.runtime,
                        input.workdir,
                        |agent, release, digest| {
                            self.inner.executor.runtime_options(agent, release, digest)
                        },
                    )?,
                )?)
            }
            "runtime/instances" => {
                let input: RuntimeInstanceListParams = params(request.params)?;
                if !self.inner.agents.contains_key(&input.agent_id) {
                    return Err(error(NOT_FOUND, "configured agent not found"));
                }
                Ok(serde_json::to_value(self.inner.store.instances(
                    input,
                    |agent, release, digest| {
                        self.inner.executor.runtime_options(agent, release, digest)
                    },
                )?)?)
            }
            "thread/list" => Ok(serde_json::to_value(
                self.inner.store.list_threads(params(request.params)?)?,
            )?),
            "thread/read" => {
                let input: ThreadReadParams = params(request.params)?;
                Ok(serde_json::to_value(self.inner.store.read_thread(input)?)?)
            }
            "turn/start" => {
                let input: TurnStartParams = params(request.params)?;
                if input.input.trim().is_empty()
                    || input.input.len() > self.inner.limits.max_input_bytes
                    || input.idempotency_key.is_empty()
                    || input.idempotency_key.len() > 128
                    || input.idempotency_key.chars().any(char::is_control)
                {
                    return Err(error(
                        INVALID_PARAMS,
                        "invalid turn input or idempotency key",
                    ));
                }
                let hash = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(
                        &json!({"thread_id":input.thread_id,"input":input.input})
                    )?)
                );
                let mut state = self.inner.scheduling.lock().unwrap();
                // Retries retain their original result even when the queue is full or a turn has finished.
                if let Some(existing) = self.inner.store.existing_turn(
                    &input.thread_id,
                    &input.idempotency_key,
                    &hash,
                )? {
                    return Ok(serde_json::to_value(existing)?);
                }
                if state.stopping || self.is_shutting_down() {
                    return Err(error(UNAVAILABLE, "Core is shutting down"));
                }
                let thread = self.inner.store.thread(&input.thread_id)?;
                if !self.inner.executor.supports_release(
                    &thread.agent_id,
                    &thread.release_id,
                    &thread.bundle_digest,
                ) {
                    return Err(error(
                        CONFLICT,
                        "thread release is unavailable; restore its immutable release",
                    ));
                }
                if state.queue.len() >= self.inner.limits.max_queued {
                    return Err(error(OVERLOADED, "Core turn queue is full"));
                }
                let turn = self.inner.store.start_turn(&input, &hash)?;
                state.queue.push_back(Job {
                    thread_id: turn.thread_id.clone(),
                    turn_id: turn.id.clone(),
                    runtime_instance_id: thread.runtime_instance_id,
                    cancel: CancellationToken::new(),
                });
                self.inner.notify.notify_one();
                Ok(serde_json::to_value(turn)?)
            }
            "turn/interrupt" => {
                let input: TurnInterruptParams = params(request.params)?;
                let mut state = self.inner.scheduling.lock().unwrap();
                let position = state.queue.iter().position(|job| {
                    job.turn_id == input.turn_id && job.thread_id == input.thread_id
                });
                let turn =
                    self.inner
                        .store
                        .interrupt(&input, position.is_some(), "user_interrupted")?;
                if let Some(position) = position {
                    state.queue.remove(position);
                }
                if let Some(job) = state.running.get(&input.turn_id).cloned()
                    && job.thread_id == input.thread_id
                    && !turn.status.is_terminal()
                {
                    state
                        .interrupt_reasons
                        .insert(input.turn_id.clone(), "user_interrupted".into());
                    job.cancel.cancel();
                }
                Ok(serde_json::to_value(turn)?)
            }
            "event/list" => Ok(serde_json::to_value(
                self.inner.store.events(params(request.params)?)?,
            )?),
            "item/list" => Ok(serde_json::to_value(
                self.inner.store.items(params(request.params)?)?,
            )?),
            _ => Err(error(METHOD_NOT_FOUND, "method not found")),
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutdown_started.load(Ordering::Acquire)
    }

    /// Fatal persistence errors stop the transport instead of reporting a healthy idle process.
    pub fn failure_token(&self) -> CancellationToken {
        self.inner.failure.clone()
    }

    /// Cancellation-safe and idempotent: dropping an RPC waiter never abandons cleanup.
    pub async fn shutdown(&self) -> Result<()> {
        if !self.inner.shutdown_started.swap(true, Ordering::AcqRel) {
            let this = self.clone();
            tokio::spawn(async move {
                let result = std::panic::AssertUnwindSafe(this.shutdown_inner())
                    .catch_unwind()
                    .await
                    .map_err(|_| "Core shutdown task failed".to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()));
                *this.inner.shutdown_result.lock().unwrap() = Some(result);
                this.inner.shutdown_done.notify_waiters();
            });
        }
        loop {
            let notified = self.inner.shutdown_done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.inner.shutdown_result.lock().unwrap().clone() {
                return result.map_err(anyhow::Error::msg);
            }
            notified.await;
        }
    }

    /// Stop admissions, cancel work, persist interrupted states, and await bounded cleanup.
    async fn shutdown_inner(&self) -> Result<()> {
        let inner = self.inner.clone();
        let initial_persistence = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut state = inner.scheduling.lock().unwrap();
            state.stopping = true;
            let running: Vec<_> = state.running.values().cloned().collect();
            for job in &running {
                state
                    .interrupt_reasons
                    .entry(job.turn_id.clone())
                    .or_insert_with(|| "core_shutdown".into());
                job.cancel.cancel();
            }
            inner.notify.notify_waiters();
            // Cancellation precedes database writes, including when persistence is unavailable.
            for job in state.queue.drain(..) {
                inner.store.interrupt(
                    &TurnInterruptParams {
                        thread_id: job.thread_id,
                        turn_id: job.turn_id,
                    },
                    true,
                    "core_shutdown",
                )?;
            }
            for job in running {
                inner.store.interrupt(
                    &TurnInterruptParams {
                        thread_id: job.thread_id,
                        turn_id: job.turn_id,
                    },
                    false,
                    "core_shutdown",
                )?;
            }
            Ok(())
        })
        .await;
        let deadline = tokio::time::Instant::now() + self.inner.limits.shutdown_grace;
        let handles = std::mem::take(&mut *self.inner.workers.lock().await);
        for mut handle in handles {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                let _ = handle.await;
            }
        }
        let inner = self.inner.clone();
        let final_persistence = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut state = inner.scheduling.lock().unwrap();
            for id in state.running.keys() {
                inner.store.finish(id, None, Some("shutdown_deadline"))?;
            }
            state.running.clear();
            state.interrupt_reasons.clear();
            Ok(())
        })
        .await;
        initial_persistence??;
        final_persistence??;
        Ok(())
    }
}

type ScheduledTurn = (Job, Thread, Turn, Vec<Value>, Option<String>);

/// Limit a notification registration to the queue wait itself. A busy executor
/// must not consume notify_one calls intended to wake another, idle worker.
async fn next_scheduled_turn(inner: &Arc<Inner>) -> Option<ScheduledTurn> {
    loop {
        let notified = inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let next = {
            let inner = inner.clone();
            tokio::task::spawn_blocking(move || -> Result<Option<ScheduledTurn>> {
                let mut state = inner.scheduling.lock().unwrap();
                if state.stopping || inner.shutdown_started.load(Ordering::Acquire) {
                    return Ok(None);
                }
                // Skip waiting instances so a queued sibling cannot occupy an
                // idle worker or block independent sandboxes behind it.
                while let Some(position) = state.queue.iter().position(|job| {
                    !state
                        .running
                        .values()
                        .any(|running| running.runtime_instance_id == job.runtime_instance_id)
                }) {
                    let job = state.queue.remove(position).expect("position found");
                    if let Some((thread, turn, history, recovery_note)) =
                        inner.store.begin(&job.turn_id)?
                    {
                        ensure!(
                            thread.runtime_instance_id == job.runtime_instance_id,
                            "queued instance binding changed"
                        );
                        state.running.insert(job.turn_id.clone(), job.clone());
                        return Ok(Some((job, thread, turn, history, recovery_note)));
                    }
                }
                Ok(None)
            })
            .await
        };
        match next {
            Ok(Ok(Some(ready))) => return Some(ready),
            Ok(Ok(None)) => {
                if inner.scheduling.lock().unwrap().stopping {
                    return None;
                }
                notified.await;
                continue;
            }
            _ => {
                // A database failure must not silently permit more external side effects.
                let mut state = inner.scheduling.lock().unwrap();
                state.stopping = true;
                for job in state.running.values() {
                    job.cancel.cancel();
                }
                inner.notify.notify_waiters();
                eprintln!("Core worker stopped after a persistence failure");
                inner.failure.cancel();
                return None;
            }
        }
    }
}

async fn worker(inner: Arc<Inner>) {
    while let Some((job, thread, turn, history, recovery_note)) = next_scheduled_turn(&inner).await
    {
        let context = ExecutionContext {
            thread_id: thread.id.clone(),
            turn_id: turn.id.clone(),
            agent_id: thread.agent_id.clone(),
            bundle_digest: thread.bundle_digest.clone(),
            release_id: thread.release_id.clone(),
            sandbox_id: thread.sandbox_id,
            runtime_instance_id: thread.runtime_instance_id,
            runtime_image: thread.runtime_image,
            runtime_provider: thread.runtime_provider,
            workdir: thread.workdir,
            input: turn.input,
            history,
            recovery_note,
            cancel: job.cancel.clone(),
            events: EventSink {
                store: Arc::downgrade(&inner.store),
                thread_id: thread.id,
                turn_id: turn.id,
            },
        };
        let compatible = inner.executor.supports_release(
            &context.agent_id,
            &context.release_id,
            &context.bundle_digest,
        );
        let result = if compatible && !job.cancel.is_cancelled() {
            std::panic::AssertUnwindSafe(inner.executor.execute(context))
                .catch_unwind()
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };
        let result = result.filter(|result| {
            result.answer.len() <= inner.limits.max_output_bytes
                && serde_json::to_vec(&result.history)
                    .is_ok_and(|history| history.len() <= inner.limits.max_history_bytes)
        });
        let inner_finish = inner.clone();
        let finished = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut state = inner_finish.scheduling.lock().unwrap();
            let reason = state.interrupt_reasons.remove(&job.turn_id);
            inner_finish
                .store
                .finish(&job.turn_id, result.as_ref(), reason.as_deref())?;
            state.running.remove(&job.turn_id);
            inner_finish.notify.notify_waiters();
            Ok(())
        })
        .await;
        if !matches!(finished, Ok(Ok(()))) {
            let mut state = inner.scheduling.lock().unwrap();
            state.stopping = true;
            for job in state.running.values() {
                job.cancel.cancel();
            }
            inner.notify.notify_waiters();
            eprintln!("Core worker stopped after a completion persistence failure");
            inner.failure.cancel();
            return;
        }
    }
}
