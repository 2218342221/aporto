use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, bail};
use aporto_core::{CoreLimits, CoreService, ExecutionContext, ExecutionResult, TurnExecutor};
use aporto_protocol::*;
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tokio::sync::Semaphore;

#[derive(Clone, Debug)]
struct Observed {
    release_id: String,
    input: String,
    history: Vec<Value>,
    sandbox_id: Option<String>,
    runtime_instance_id: String,
    runtime_image: String,
    runtime_provider: String,
    workdir: String,
    recovery_note: Option<String>,
}

struct FakeExecutor {
    digest: String,
    archive: Option<String>,
    calls: AtomicUsize,
    contexts: Mutex<Vec<Observed>>,
    permits: Semaphore,
}

impl FakeExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            digest: format!("sha256:{}", "a".repeat(64)),
            archive: None,
            calls: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
            permits: Semaphore::new(0),
        })
    }
}

#[async_trait]
impl TurnExecutor for FakeExecutor {
    fn agents(&self) -> Vec<AgentSummary> {
        vec![AgentSummary {
            id: "test".into(),
            name: "Test".into(),
            model: "fixture".into(),
            bundle_digest: self.digest.clone(),
            release_id: self.digest.clone(),
            runtime: RuntimeOptions {
                provider: "docker".into(),
                images: vec!["fixture:latest".into(), "fixture:alternate".into()],
                default_image: "fixture:latest".into(),
                default_workdir: "/workspace".into(),
            },
        }]
    }
    fn supports_release(&self, agent_id: &str, release_id: &str, bundle_digest: &str) -> bool {
        agent_id == "test"
            && release_id == bundle_digest
            && (release_id == self.digest || self.archive.as_deref() == Some(release_id))
    }
    fn runtime_options(
        &self,
        agent_id: &str,
        release_id: &str,
        bundle_digest: &str,
    ) -> Option<RuntimeOptions> {
        self.supports_release(agent_id, release_id, bundle_digest)
            .then(|| self.agents().remove(0).runtime)
    }
    async fn execute(&self, context: ExecutionContext) -> Result<ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(Observed {
            release_id: context.release_id.clone(),
            input: context.input.clone(),
            history: context.history.clone(),
            sandbox_id: context.sandbox_id.clone(),
            runtime_instance_id: context.runtime_instance_id.clone(),
            runtime_image: context.runtime_image.clone(),
            runtime_provider: context.runtime_provider.clone(),
            workdir: context.workdir.clone(),
            recovery_note: context.recovery_note.clone(),
        });
        context
            .events
            .emit(
                "runtime.ready",
                json!({"sandbox_id":context.sandbox_id.clone().unwrap_or_else(||format!("sandbox-{}", context.thread_id))}),
            )
            .await?;
        match context.input.as_str() {
            "block" => {
                tokio::select! {
                    permit = self.permits.acquire() => permit?.forget(),
                    _ = context.cancel.cancelled() => bail!("cancelled"),
                }
            }
            "cancel-then-cleanup" => {
                context.cancel.cancelled().await;
                self.permits.acquire().await?.forget();
                bail!("cancelled after cleanup");
            }
            "ignore-cancel" => std::future::pending::<()>().await,
            "fail" => bail!("a sensitive executor error must not be persisted"),
            "panic" => panic!("fixture panic"),
            "huge-output" => {
                return Ok(ExecutionResult {
                    answer: "x".repeat(32 * 1024 + 1),
                    history: vec![],
                });
            }
            "huge-history" => {
                return Ok(ExecutionResult {
                    answer: "small".into(),
                    history: vec![json!("x".repeat(8 * 1024 * 1024))],
                });
            }
            "flood" => {
                for _ in 0..100 {
                    context.events.emit("tool.progress", json!({})).await?;
                }
            }
            "large-events" => {
                for _ in 0..10 {
                    context
                        .events
                        .emit("tool.progress", json!({"text":"x".repeat(200 * 1024)}))
                        .await?;
                }
            }
            "reserved-event" => {
                context
                    .events
                    .emit("turn.completed", json!({"output":"forged"}))
                    .await?
            }
            "reserved-start-event" => context.events.emit("turn.started", json!({})).await?,
            "transcript" => {
                context
                    .events
                    .emit(
                        "item.started",
                        json!({"item":{
                    "id":"message-1","kind":"assistant_message","status":"in_progress",
                    "phase":"commentary","text":"Inspecting"}}),
                    )
                    .await?;
                context
                    .events
                    .emit(
                        "item.updated",
                        json!({"item":{
                    "id":"message-1","kind":"assistant_message","status":"in_progress",
                    "phase":"commentary","text":"Inspecting workspace"}}),
                    )
                    .await?;
                context
                    .events
                    .emit(
                        "item.completed",
                        json!({"item":{
                    "id":"message-1","kind":"assistant_message","status":"completed",
                    "phase":"final_answer","text":"Inspected workspace"}}),
                    )
                    .await?;
            }
            _ => {}
        }
        let answer = format!("answer:{}", context.input);
        let mut history = context.history;
        history.push(json!({"role":"user","content":context.input}));
        history.push(json!({"role":"assistant","content":answer}));
        Ok(ExecutionResult { answer, history })
    }
}

#[tokio::test]
async fn item_list_reads_durable_snapshots_separately_from_turn_history() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let handshake: InitializeResult = call(
        &core,
        "initialize",
        json!({"protocol_version":PROTOCOL_VERSION,"client_name":"items-test"}),
    )
    .await;
    assert!(
        handshake
            .capabilities
            .iter()
            .any(|capability| capability == "item/list")
    );
    let thread = thread(&core).await;
    let turn = start(&core, &thread, "transcript", "one").await;
    assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Completed);
    let params = json!({"thread_id":thread.id,"turn_id":turn.id,"after":0,"limit":100});
    let items: ItemListResult = call(&core, "item/list", params.clone()).await;
    assert_eq!(items.items.len(), 1);
    assert_eq!(items.items[0].content.status, ItemStatus::Completed);
    assert_eq!(
        items.items[0].content.text.as_deref(),
        Some("Inspected workspace")
    );
    assert_eq!(items.items[0].content.phase, Some(ItemPhase::FinalAnswer));
    assert!(items.items[0].sequence > items.items[0].ordinal);
    let snapshot: Value = call(&core, "thread/read", json!({"thread_id":thread.id})).await;
    assert!(snapshot["turns"][0].get("items").is_none());
    let invalid = response(
        &core,
        "item/list",
        json!({"thread_id":thread.id,"turn_id":turn.id,"after":u64::MAX}),
    )
    .await;
    assert_eq!(invalid.error.unwrap().code, INVALID_PARAMS);
    assert!(!core.failure_token().is_cancelled());
    core.shutdown().await.unwrap();
    drop(core);
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let restored: ItemListResult = call(&core, "item/list", params).await;
    assert_eq!(restored.items, items.items);
    core.shutdown().await.unwrap();
}

async fn response(core: &CoreService, method: &str, params: impl Serialize) -> RpcResponse {
    core.handle(RpcRequest {
        jsonrpc: "2.0".into(),
        id: 1,
        method: method.into(),
        params: serde_json::to_value(params).unwrap(),
    })
    .await
}

async fn call<T: DeserializeOwned>(core: &CoreService, method: &str, params: impl Serialize) -> T {
    let reply = response(core, method, params).await;
    assert!(reply.error.is_none(), "RPC error: {:?}", reply.error);
    serde_json::from_value(reply.result.unwrap()).unwrap()
}

async fn initialized(
    directory: &TempDir,
    executor: Arc<FakeExecutor>,
    limits: CoreLimits,
) -> CoreService {
    let core = CoreService::open(directory.path(), executor, limits)
        .await
        .unwrap();
    let _: InitializeResult = call(
        &core,
        "initialize",
        json!({"protocol_version":PROTOCOL_VERSION,"client_name":"test"}),
    )
    .await;
    core
}

async fn thread(core: &CoreService) -> Thread {
    call(core, "thread/start", json!({"agent_id":"test"})).await
}
async fn start(core: &CoreService, thread: &Thread, input: &str, key: &str) -> Turn {
    call(
        core,
        "turn/start",
        json!({"thread_id":thread.id,"input":input,"idempotency_key":key}),
    )
    .await
}

async fn read(core: &CoreService, thread_id: &str) -> ThreadReadResult {
    call(core, "thread/read", json!({"thread_id":thread_id})).await
}

async fn terminal(core: &CoreService, turn: &Turn) -> Turn {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let found = read(core, &turn.thread_id)
                .await
                .turns
                .into_iter()
                .find(|item| item.id == turn.id)
                .unwrap();
            if found.status.is_terminal() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
    })
    .await
    .expect("turn did not become terminal")
}

async fn wait_calls(executor: &FakeExecutor, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while executor.calls.load(Ordering::SeqCst) < count {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn protocol_requires_compatible_initialization_and_strict_parameters() {
    let directory = tempdir().unwrap();
    let core = CoreService::open(directory.path(), FakeExecutor::new(), CoreLimits::default())
        .await
        .unwrap();
    assert_eq!(
        response(&core, "agent/list", json!({}))
            .await
            .error
            .unwrap()
            .code,
        UNAVAILABLE
    );
    assert_eq!(
        response(
            &core,
            "initialize",
            json!({"protocol_version":"99","client_name":"test"})
        )
        .await
        .error
        .unwrap()
        .code,
        CONFLICT
    );
    let init: InitializeResult = call(
        &core,
        "initialize",
        json!({"protocol_version":PROTOCOL_VERSION,"client_name":"test"}),
    )
    .await;
    assert_eq!(init.protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        response(
            &core,
            "thread/start",
            json!({"agent_id":"test","path":"/secret"})
        )
        .await
        .error
        .unwrap()
        .code,
        INVALID_PARAMS
    );
    assert_eq!(
        response(&core, "thread/start", json!({"agent_id":"unknown"}))
            .await
            .error
            .unwrap()
            .code,
        NOT_FOUND
    );
    assert_eq!(
        response(&core, "unknown", json!({}))
            .await
            .error
            .unwrap()
            .code,
        METHOD_NOT_FOUND
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_idempotent_requests_execute_once_and_conflicting_payloads_fail() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(&directory, executor.clone(), CoreLimits::default()).await;
    let thread = thread(&core).await;
    let request = json!({"thread_id":thread.id,"input":"block","idempotency_key":"same-key"});
    let (one, two) = tokio::join!(
        response(&core, "turn/start", request.clone()),
        response(&core, "turn/start", request.clone())
    );
    let one: Turn = serde_json::from_value(one.result.unwrap()).unwrap();
    let two: Turn = serde_json::from_value(two.result.unwrap()).unwrap();
    assert_eq!(one.id, two.id);
    wait_calls(&executor, 1).await;
    let conflicting =
        json!({"thread_id":thread.id,"input":"different","idempotency_key":"same-key"});
    assert_eq!(
        response(&core, "turn/start", conflicting)
            .await
            .error
            .unwrap()
            .code,
        CONFLICT
    );
    assert_eq!(
        response(
            &core,
            "turn/start",
            json!({"thread_id":thread.id,"input":"another","idempotency_key":"new-key"})
        )
        .await
        .error
        .unwrap()
        .code,
        CONFLICT
    );
    executor.permits.add_permits(1);
    assert_eq!(terminal(&core, &one).await.status, TurnStatus::Completed);
    let retry: Turn = call(&core, "turn/start", request).await;
    assert_eq!(retry.id, one.id);
    assert_eq!(retry.status, TurnStatus::Completed);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_queued_turn_frees_capacity_without_dispatching_it() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(
        &directory,
        executor.clone(),
        CoreLimits {
            max_concurrent: 1,
            max_queued: 1,
            ..CoreLimits::default()
        },
    )
    .await;
    let first_thread = thread(&core).await;
    let second_thread = thread(&core).await;
    let third_thread = thread(&core).await;
    let running = start(&core, &first_thread, "block", "1").await;
    wait_calls(&executor, 1).await;
    let queued = start(&core, &second_thread, "never-run", "2").await;
    assert_eq!(
        response(
            &core,
            "turn/start",
            json!({"thread_id":third_thread.id,"input":"third","idempotency_key":"3"})
        )
        .await
        .error
        .unwrap()
        .code,
        OVERLOADED
    );
    let interrupted: Turn = call(
        &core,
        "turn/interrupt",
        json!({"thread_id":second_thread.id,"turn_id":queued.id}),
    )
    .await;
    assert_eq!(interrupted.status, TurnStatus::Interrupted);
    let third = start(&core, &third_thread, "third", "3").await;
    let _: Turn = call(
        &core,
        "turn/interrupt",
        json!({"thread_id":first_thread.id,"turn_id":running.id}),
    )
    .await;
    assert_eq!(
        terminal(&core, &running).await.status,
        TurnStatus::Interrupted
    );
    assert_eq!(terminal(&core, &third).await.status, TurnStatus::Completed);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert!(
        !executor
            .contexts
            .lock()
            .unwrap()
            .iter()
            .any(|context| context.input == "never-run")
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn worker_concurrency_is_bounded() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(
        &directory,
        executor.clone(),
        CoreLimits {
            max_concurrent: 2,
            ..CoreLimits::default()
        },
    )
    .await;
    let mut turns = Vec::new();
    for number in 0..3 {
        let thread = thread(&core).await;
        turns.push(start(&core, &thread, "block", &number.to_string()).await);
    }
    wait_calls(&executor, 2).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    executor.permits.add_permits(1);
    wait_calls(&executor, 3).await;
    executor.permits.add_permits(2);
    for turn in turns {
        assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Completed);
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn successful_history_and_runtime_identity_survive_restart_and_retry() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(&directory, executor, CoreLimits::default()).await;
    let thread = thread(&core).await;
    let first = start(&core, &thread, "first", "1").await;
    terminal(&core, &first).await;
    let persisted = read(&core, &thread.id).await.thread;
    assert_eq!(
        persisted.sandbox_id.as_deref(),
        Some(format!("sandbox-{}", thread.id).as_str())
    );
    core.shutdown().await.unwrap();
    drop(core);
    let executor = FakeExecutor::new();
    let core = initialized(&directory, executor.clone(), CoreLimits::default()).await;
    let retry = start(&core, &thread, "first", "1").await;
    assert_eq!(retry.id, first.id);
    assert_eq!(retry.status, TurnStatus::Completed);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let second = start(&core, &thread, "second", "2").await;
    terminal(&core, &second).await;
    let observed = executor.contexts.lock().unwrap()[0].clone();
    assert_eq!(observed.history.len(), 2);
    assert_eq!(observed.sandbox_id, persisted.sandbox_id);
    assert!(observed.recovery_note.is_none());
    let events: EventListResult = call(
        &core,
        "event/list",
        json!({"thread_id":thread.id,"after":persisted.last_sequence}),
    )
    .await;
    assert!(
        events
            .events
            .iter()
            .all(|event| event.sequence > persisted.last_sequence)
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_turn_history_is_not_replayed_and_recovery_warning_is_provided() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(&directory, executor.clone(), CoreLimits::default()).await;
    let thread = thread(&core).await;
    let first = start(&core, &thread, "first", "1").await;
    terminal(&core, &first).await;
    let failure = start(&core, &thread, "fail", "2").await;
    let failed = terminal(&core, &failure).await;
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("execution_failed"));
    let next = start(&core, &thread, "next", "3").await;
    terminal(&core, &next).await;
    let contexts = executor.contexts.lock().unwrap().clone();
    assert_eq!(contexts[2].history, contexts[1].history);
    assert_eq!(contexts[2].history.len(), 2);
    assert!(
        contexts[2]
            .recovery_note
            .as_deref()
            .unwrap()
            .contains("side effects")
    );
    drop(contexts);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_interrupts_queued_and_running_turns_and_stops_admissions() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(
        &directory,
        executor.clone(),
        CoreLimits {
            max_concurrent: 1,
            ..CoreLimits::default()
        },
    )
    .await;
    let first = thread(&core).await;
    let second = thread(&core).await;
    let running = start(&core, &first, "block", "1").await;
    wait_calls(&executor, 1).await;
    let queued = start(&core, &second, "queued", "2").await;
    core.shutdown().await.unwrap();
    assert_eq!(
        terminal(&core, &running).await.status,
        TurnStatus::Interrupted
    );
    assert_eq!(
        terminal(&core, &queued).await.status,
        TurnStatus::Interrupted
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        response(&core, "thread/start", json!({"agent_id":"test"}))
            .await
            .error
            .unwrap()
            .code,
        UNAVAILABLE
    );
}

#[tokio::test]
async fn shutdown_deadline_aborts_an_uncooperative_executor_and_persists_interruption() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(
        &directory,
        executor.clone(),
        CoreLimits {
            max_concurrent: 1,
            shutdown_grace: Duration::from_millis(20),
            ..CoreLimits::default()
        },
    )
    .await;
    let thread = thread(&core).await;
    let turn = start(&core, &thread, "ignore-cancel", "1").await;
    wait_calls(&executor, 1).await;
    let waiter_core = core.clone();
    let waiter = tokio::spawn(async move { waiter_core.shutdown().await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    waiter.abort(); // A disconnected transport must not cancel the cleanup supervisor.
    tokio::time::timeout(Duration::from_secs(1), core.shutdown())
        .await
        .unwrap()
        .unwrap();
    let interrupted = terminal(&core, &turn).await;
    assert_eq!(interrupted.status, TurnStatus::Interrupted);
    assert_eq!(interrupted.error.as_deref(), Some("shutdown_deadline"));
}

#[tokio::test]
async fn state_directory_has_one_owner_and_private_sqlite_wal_files() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    assert!(
        CoreService::open(directory.path(), FakeExecutor::new(), CoreLimits::default())
            .await
            .is_err()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for name in [
            "state.lock",
            "state.sqlite",
            "state.sqlite-wal",
            "state.sqlite-shm",
        ] {
            assert_eq!(
                std::fs::metadata(directory.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    core.shutdown().await.unwrap();
    drop(core);
    let reopened = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn event_replay_pages_have_monotonic_cursors_and_a_wire_byte_budget() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let thread = thread(&core).await;
    let turn = start(&core, &thread, "large-events", "1").await;
    assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Completed);
    let mut after = 0;
    let mut all = Vec::new();
    loop {
        let page: EventListResult = call(
            &core,
            "event/list",
            json!({"thread_id":thread.id,"after":after,"limit":500}),
        )
        .await;
        assert!(serde_json::to_vec(&page).unwrap().len() < MAX_FRAME_BYTES);
        assert!(!page.events.is_empty());
        assert!(page.events.iter().all(|event| event.sequence > after));
        assert!(page.next_cursor > after);
        after = page.next_cursor;
        all.extend(page.events);
        if !page.has_more {
            break;
        }
    }
    assert_eq!(all.first().unwrap().kind, "thread.started");
    assert_eq!(all.last().unwrap().kind, "turn.completed");
    assert_eq!(
        all.iter()
            .filter(|event| event.kind == "tool.progress")
            .count(),
        10
    );
    assert!(
        all.windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn event_and_executor_output_limits_fail_without_corrupting_worker_or_history() {
    let directory = tempdir().unwrap();
    let core = initialized(
        &directory,
        FakeExecutor::new(),
        CoreLimits {
            max_events_per_turn: 16,
            ..CoreLimits::default()
        },
    )
    .await;
    let thread = thread(&core).await;
    for (index, input) in [
        "flood",
        "reserved-event",
        "reserved-start-event",
        "huge-output",
        "huge-history",
        "panic",
    ]
    .iter()
    .enumerate()
    {
        let turn = start(&core, &thread, input, &index.to_string()).await;
        assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Failed);
    }
    let healthy = start(&core, &thread, "healthy", "healthy").await;
    assert_eq!(
        terminal(&core, &healthy).await.status,
        TurnStatus::Completed
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn thread_and_turn_limits_and_cross_thread_interrupt_are_enforced() {
    let directory = tempdir().unwrap();
    let core = initialized(
        &directory,
        FakeExecutor::new(),
        CoreLimits {
            max_threads: 2,
            ..CoreLimits::default()
        },
    )
    .await;
    let first = thread(&core).await;
    let second = thread(&core).await;
    assert_eq!(
        response(&core, "thread/start", json!({"agent_id":"test"}))
            .await
            .error
            .unwrap()
            .code,
        OVERLOADED
    );
    assert_eq!(
        response(
            &core,
            "turn/start",
            json!({"thread_id":first.id,"input":"x".repeat(32*1024+1),"idempotency_key":"big"})
        )
        .await
        .error
        .unwrap()
        .code,
        INVALID_PARAMS
    );
    let turn = start(&core, &first, "block", "1").await;
    assert_eq!(
        response(
            &core,
            "turn/interrupt",
            json!({"thread_id":second.id,"turn_id":turn.id})
        )
        .await
        .error
        .unwrap()
        .code,
        NOT_FOUND
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn turn_pagination_has_no_gaps_or_duplicate_turns() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let thread = thread(&core).await;
    let mut ids = Vec::new();
    for index in 0..25 {
        let turn = start(&core, &thread, &format!("input{index}"), &index.to_string()).await;
        terminal(&core, &turn).await;
        ids.push(turn.id);
    }
    let recent: ThreadReadResult = call(&core, "thread/read", json!({"thread_id":thread.id})).await;
    assert_eq!(
        recent.turns.iter().map(|turn| &turn.id).collect::<Vec<_>>(),
        ids[5..].iter().collect::<Vec<_>>()
    );
    let older: ThreadReadResult = call(
        &core,
        "thread/read",
        json!({"thread_id":thread.id,"before":recent.next_cursor}),
    )
    .await;
    assert_eq!(
        older.turns.iter().map(|turn| &turn.id).collect::<Vec<_>>(),
        ids[..5].iter().collect::<Vec<_>>()
    );
    assert!(older.next_cursor.is_none());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_rpc_waits_for_cleanup_and_is_idempotent() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(&directory, executor.clone(), CoreLimits::default()).await;
    assert_eq!(
        response(&core, "shutdown", json!({"unknown":true}))
            .await
            .error
            .unwrap()
            .code,
        INVALID_PARAMS
    );
    let thread = thread(&core).await;
    let turn = start(&core, &thread, "block", "1").await;
    wait_calls(&executor, 1).await;
    let stopped: Value = call(&core, "shutdown", json!({})).await;
    assert_eq!(stopped, json!({"status":"stopped"}));
    assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Interrupted);
    let before: EventListResult = call(&core, "event/list", json!({"thread_id":thread.id})).await;
    let again: Value = call(&core, "shutdown", json!({})).await;
    assert_eq!(again, stopped);
    let after: EventListResult = call(&core, "event/list", json!({"thread_id":thread.id})).await;
    assert_eq!(after.events.len(), before.events.len());
}

#[tokio::test]
async fn changed_bundle_cannot_execute_an_existing_thread_after_restart() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let thread = thread(&core).await;
    core.shutdown().await.unwrap();
    drop(core);
    let mut replacement = FakeExecutor::new();
    Arc::get_mut(&mut replacement).unwrap().digest = format!("sha256:{}", "b".repeat(64));
    let core = initialized(&directory, replacement.clone(), CoreLimits::default()).await;
    assert_eq!(
        response(
            &core,
            "turn/start",
            json!({"thread_id":thread.id,"input":"new","idempotency_key":"1"})
        )
        .await
        .error
        .unwrap()
        .code,
        CONFLICT
    );
    assert_eq!(replacement.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        read(&core, &thread.id).await.thread.bundle_digest,
        thread.bundle_digest
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn existing_thread_uses_archived_release_after_active_release_changes() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let existing = thread(&core).await;
    core.shutdown().await.unwrap();
    drop(core);
    let mut replacement = FakeExecutor::new();
    let executor = Arc::get_mut(&mut replacement).unwrap();
    executor.archive = Some(existing.release_id.clone());
    executor.digest = format!("sha256:{}", "b".repeat(64));
    let core = initialized(&directory, replacement.clone(), CoreLimits::default()).await;
    let new_thread = thread(&core).await;
    assert_ne!(new_thread.release_id, existing.release_id);
    let turn = start(&core, &existing, "continue", "1").await;
    assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Completed);
    assert_eq!(
        replacement.contexts.lock().unwrap()[0].release_id,
        existing.release_id
    );
    assert_eq!(
        read(&core, &existing.id).await.thread.release_id,
        existing.release_id
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn escaped_turn_content_is_paginated_by_encoded_size() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let thread = thread(&core).await;
    let input = "\u{1}".repeat(32 * 1024 - 8);
    for index in 0..5 {
        let turn = start(&core, &thread, &input, &index.to_string()).await;
        assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Completed);
    }
    let page = read(&core, &thread.id).await;
    assert!(page.turns.len() < 5);
    assert!(page.next_cursor.is_some());
    assert!(serde_json::to_vec(&page).unwrap().len() < MAX_FRAME_BYTES);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_start_event_matches_the_public_progress_contract() {
    let directory = tempdir().unwrap();
    let core = initialized(&directory, FakeExecutor::new(), CoreLimits::default()).await;
    let thread = thread(&core).await;
    let turn = start(&core, &thread, "healthy", "1").await;
    assert_eq!(terminal(&core, &turn).await.status, TurnStatus::Completed);
    let events: EventListResult = call(&core, "event/list", json!({"thread_id":thread.id})).await;
    let lifecycle: Vec<_> = events
        .events
        .iter()
        .filter(|event| event.kind.starts_with("turn."))
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(lifecycle, ["turn.queued", "turn.started", "turn.completed"]);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn busy_worker_does_not_consume_notifications_for_an_idle_worker() {
    let directory = tempdir().unwrap();
    let executor = FakeExecutor::new();
    let core = initialized(
        &directory,
        executor.clone(),
        CoreLimits {
            max_concurrent: 2,
            ..CoreLimits::default()
        },
    )
    .await;
    let first_thread = thread(&core).await;
    let second_thread = thread(&core).await;
    let third_thread = thread(&core).await;
    let first = start(&core, &first_thread, "block", "first").await;
    let second = start(&core, &second_thread, "block", "second").await;
    wait_calls(&executor, 2).await;
    executor.permits.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let a = read(&core, &first.thread_id).await.turns[0].status;
            let b = read(&core, &second.thread_id).await.turns[0].status;
            if [a, b].contains(&TurnStatus::Completed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    // The completed worker returns to waiting while the other still executes.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let third = start(&core, &third_thread, "healthy", "third").await;
    let completed = tokio::time::timeout(Duration::from_secs(1), terminal(&core, &third)).await;
    // Always release the deliberately blocked executor, even if the assertion fails.
    core.shutdown().await.unwrap();
    assert_eq!(
        completed
            .expect("new work was stranded behind the busy worker")
            .status,
        TurnStatus::Completed
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 3);
}

#[path = "service/runtime_instances.rs"]
mod runtime_instances;
