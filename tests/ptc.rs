//! These tests execute real QuickJS runtimes. Only the external tool service is a fixture.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use aporto::ptc::{
    CellResult, CellStatus, MAX_ACTIVE_CELLS, MAX_PENDING_CALLBACKS, MAX_SOURCE_BYTES,
    ObserveOptions, PtcSession,
};
use aporto::types::{Limits, ToolBroker, ToolDefinition};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Fixture {
    active: AtomicUsize,
    peak: AtomicUsize,
    calls: AtomicUsize,
    completed: AtomicUsize,
}

struct ActiveCall<'a>(&'a AtomicUsize);
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ToolBroker for Fixture {
    fn definitions(&self) -> Vec<ToolDefinition> {
        ["echo", "delay", "serial", "fail"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("fixture {name}"),
                input_schema: json!({"type":"object"}),
                parallel: name != "serial",
            })
            .collect()
    }

    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveCall(&self.active);
        self.peak.fetch_max(active, Ordering::SeqCst);
        match name {
            "echo" => Ok(args),
            "delay" | "serial" => {
                let ms = args["ms"].as_u64().unwrap_or(30);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => bail!("fixture cancelled"),
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                }
                self.completed.fetch_add(1, Ordering::SeqCst);
                Ok(args["value"].clone())
            }
            "fail" => bail!("fixture failure"),
            _ => bail!("unknown fixture tool"),
        }
    }
}

fn limits() -> Limits {
    Limits {
        cell_timeout_ms: 2000,
        memory_mb: 16,
        max_output_bytes: 8192,
        max_tool_calls: 16,
        max_parallel: 4,
        ..Limits::default()
    }
}

fn observe() -> ObserveOptions {
    ObserveOptions {
        yield_time_ms: Some(1500),
        max_output_bytes: None,
    }
}

fn session() -> (PtcSession, Arc<Fixture>) {
    let broker = Arc::new(Fixture::default());
    (PtcSession::new(broker.clone(), limits()), broker)
}

async fn exec(session: &PtcSession, source: &str) -> CellResult {
    tokio::time::timeout(Duration::from_secs(5), session.exec(source, observe()))
        .await
        .expect("runtime must respond")
        .expect("exec admission")
}

#[tokio::test]
async fn fresh_globals_and_json_kv_are_separate() {
    let (session, _) = session();
    let first = exec(
        &session,
        r#"globalThis.temporary = 42; store('answer', {n:42}); text(load('answer'));"#,
    )
    .await;
    assert_eq!(first.status, CellStatus::Completed);
    assert_eq!(first.output, [r#"{"n":42}"#]);
    let second = exec(&session, r#"text(typeof temporary); const value=load('answer'); value.n=0; text(load('answer')); text(load('missing'));"#).await;
    assert_eq!(second.output, ["undefined", r#"{"n":42}"#, "undefined"]);
    let other = PtcSession::new(Arc::new(Fixture::default()), limits());
    assert_eq!(
        exec(&other, "text(load('answer'));").await.output,
        ["undefined"]
    );
    session.close().await.unwrap();
    other.close().await.unwrap();
}

#[tokio::test]
async fn no_host_capabilities_or_module_imports() {
    let (session, _) = session();
    let result = exec(&session, r#"
        text(['process','require','fetch','console','std','os','__call_json'].map(k=>typeof globalThis[k]));
        text(Function('return typeof process')());
        try { await import('node:fs'); text('unexpected'); } catch (_) { text('import blocked'); }
        text(ALL_TOOLS.find(t=>t.name==='echo').description === 'fixture echo');
    "#).await;
    assert_eq!(result.status, CellStatus::Completed, "{:?}", result.error);
    assert_eq!(
        result.output,
        [
            r#"["undefined","undefined","undefined","undefined","undefined","undefined","undefined"]"#,
            "undefined",
            "import blocked",
            "true",
        ]
    );
    let static_import = exec(&session, "import x from 'node:fs';").await;
    assert_eq!(static_import.status, CellStatus::Failed);
    session.close().await.unwrap();
}

#[tokio::test]
async fn promise_all_dispatches_real_concurrent_callbacks() {
    let (session, fixture) = session();
    let result = exec(
        &session,
        r#"
        const results = await Promise.all([1,2,3].map(value=>tools.delay({ms:80,value})));
        text(results);
    "#,
    )
    .await;
    assert_eq!(result.status, CellStatus::Completed, "{:?}", result.error);
    assert_eq!(result.output, ["[1,2,3]"]);
    assert_eq!(fixture.peak.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.completed.load(Ordering::SeqCst), 3);
    session.close().await.unwrap();
}

#[tokio::test]
async fn concurrency_limit_and_nonparallel_tools_are_enforced() {
    let fixture = Arc::new(Fixture::default());
    let mut limit = limits();
    limit.max_parallel = 2;
    let session = PtcSession::new(fixture.clone(), limit);
    let result = exec(
        &session,
        "await Promise.all([1,2,3,4].map(value=>tools.delay({ms:30,value}))); text('ok');",
    )
    .await;
    assert_eq!(result.status, CellStatus::Completed, "{:?}", result.error);
    assert_eq!(fixture.peak.load(Ordering::SeqCst), 2);
    fixture.peak.store(0, Ordering::SeqCst);
    let result = exec(
        &session,
        "await Promise.all([1,2,3].map(value=>tools.serial({ms:20,value}))); text('ok');",
    )
    .await;
    assert_eq!(result.status, CellStatus::Completed);
    assert_eq!(fixture.peak.load(Ordering::SeqCst), 1);
    session.close().await.unwrap();
}

#[tokio::test]
async fn yield_snapshots_preserve_exact_output_boundaries() {
    let (session, _) = session();
    // There is intentionally no await after yield: an observer race must not merge the chunks.
    let first = exec(&session, "text('before'); yield_control(); text('after');").await;
    assert_eq!(first.status, CellStatus::Running);
    assert_eq!(first.output, ["before"]);
    let second = session
        .wait(&first.cell_id, observe(), false)
        .await
        .unwrap();
    assert_eq!(second.status, CellStatus::Completed);
    assert_eq!(second.output, ["after"]);
    assert!(
        session
            .wait(&first.cell_id, observe(), false)
            .await
            .is_err()
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn timed_yield_does_not_stop_a_cell() {
    let (session, _) = session();
    let first = session
        .exec(
            "text('started'); await tools.delay({ms:100}); text('done');",
            ObserveOptions {
                yield_time_ms: Some(25),
                ..ObserveOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.status, CellStatus::Running);
    let second = session
        .wait(&first.cell_id, observe(), false)
        .await
        .unwrap();
    assert_eq!(second.status, CellStatus::Completed);
    let all: Vec<_> = first.output.into_iter().chain(second.output).collect();
    assert_eq!(all, ["started", "done"]);
    session.close().await.unwrap();
}

#[tokio::test]
async fn cpu_loop_is_interrupted_by_deadline() {
    let mut limit = limits();
    limit.cell_timeout_ms = 75;
    let session = PtcSession::new(Arc::new(Fixture::default()), limit);
    let start = Instant::now();
    let mut result = exec(&session, "while (true) {}").await;
    if result.status == CellStatus::Running {
        result = session
            .wait(&result.cell_id, observe(), false)
            .await
            .unwrap();
    }
    assert_eq!(result.status, CellStatus::Failed);
    assert!(result.error.unwrap().contains("deadline"));
    assert!(start.elapsed() < Duration::from_secs(2));
    session.close().await.unwrap();
}

#[tokio::test]
async fn explicit_termination_interrupts_cpu_and_discards_kv_writes() {
    let (session, _) = session();
    let first = exec(
        &session,
        "store('aborted',1); text('ready'); yield_control(); while(true) {}",
    )
    .await;
    assert_eq!(first.status, CellStatus::Running);
    let final_result = session.wait(&first.cell_id, observe(), true).await.unwrap();
    assert_eq!(final_result.status, CellStatus::Terminated);
    assert_eq!(
        exec(&session, "text(load('aborted'));").await.output,
        ["undefined"]
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn tool_cancellation_and_unawaited_work_do_not_leak() {
    let (session, fixture) = session();
    let first = exec(
        &session,
        "const pending=tools.delay({ms:1000}); yield_control(); await pending;",
    )
    .await;
    let stopped = session.wait(&first.cell_id, observe(), true).await.unwrap();
    assert_eq!(stopped.status, CellStatus::Terminated);
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    let unawaited = exec(&session, "tools.delay({ms:80}); text('finished');").await;
    assert_eq!(unawaited.status, CellStatus::Completed);
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.completed.load(Ordering::SeqCst), 0);
    session.close().await.unwrap();
}

#[tokio::test]
async fn failed_scripts_commit_normal_writes_and_tools_reject_promises() {
    let (session, _) = session();
    let result = exec(
        &session,
        "store('written',1); throw new Error('expected boom');",
    )
    .await;
    assert_eq!(result.status, CellStatus::Failed);
    assert!(result.error.unwrap().contains("expected boom"));
    assert_eq!(exec(&session, "text(load('written'));").await.output, ["1"]);
    let result = exec(&session, "const r=await Promise.allSettled([tools.fail({}), tools.echo({ok:true})]); text(r.map(x=>x.status));").await;
    assert_eq!(result.output, [r#"["rejected","fulfilled"]"#]);
    let result = exec(&session, "await tools.fail({});").await;
    assert_eq!(result.status, CellStatus::Failed);
    assert!(result.error.unwrap().contains("fixture failure"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn rejected_promises_never_alias_an_empty_success_value() {
    let (session, _) = session();
    for source in ["throw '';", "throw {toString(){ return ''; }};"] {
        let result = exec(&session, source).await;
        assert_eq!(result.status, CellStatus::Failed, "accepted {source}");
        assert!(result.error.is_some_and(|error| !error.is_empty()));
    }
    assert_eq!(
        exec(&session, "return ''; ").await.status,
        CellStatus::Completed
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn cell_ids_from_another_turn_cannot_observe_or_cancel_current_work() {
    let (first, _) = session();
    let (second, _) = session();
    let old = exec(&first, "yield_control(); await new Promise(()=>{});").await;
    let current = exec(&second, "yield_control(); await new Promise(()=>{});").await;
    assert_eq!(old.status, CellStatus::Running);
    assert_eq!(current.status, CellStatus::Running);
    assert_ne!(old.cell_id, current.cell_id);
    assert!(second.wait(&old.cell_id, observe(), true).await.is_err());
    let still_running = second
        .wait(
            &current.cell_id,
            ObserveOptions {
                yield_time_ms: Some(0),
                max_output_bytes: None,
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(still_running.status, CellStatus::Running);
    first.close().await.unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
async fn eager_fanout_rejects_excess_pending_work_and_releases_slots() {
    let fixture = Arc::new(Fixture::default());
    let mut limit = limits();
    limit.max_tool_calls = 4096;
    limit.max_parallel = 128;
    let session = PtcSession::new(fixture.clone(), limit);
    let result = exec(&session, r#"
        const results = await Promise.allSettled(Array.from({length:1024}, () => tools.delay({ms:1})));
        text(results.filter(result => result.status === 'fulfilled').length);
        text(results.filter(result => result.status === 'rejected').every(result => result.reason.message.includes('pending callback limit')));
        text(await tools.echo({recovered:true}));
    "#).await;
    assert_eq!(result.status, CellStatus::Completed, "{:?}", result.error);
    assert_eq!(
        result.output,
        [
            MAX_PENDING_CALLBACKS.to_string(),
            "true".into(),
            r#"{"recovered":true}"#.into()
        ]
    );
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        MAX_PENDING_CALLBACKS + 1
    );
    assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
    session.close().await.unwrap();
}

#[tokio::test]
async fn call_fanout_budget_is_enforced_before_broker_dispatch() {
    let fixture = Arc::new(Fixture::default());
    let mut limit = limits();
    limit.max_tool_calls = 2;
    let session = PtcSession::new(fixture.clone(), limit);
    let result = exec(&session, "const r=await Promise.allSettled([1,2,3,4].map(i=>tools.echo({i}))); text(r.map(x=>x.status));").await;
    assert_eq!(result.status, CellStatus::Completed, "{:?}", result.error);
    assert_eq!(
        result.output,
        [r#"["fulfilled","fulfilled","rejected","rejected"]"#]
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    session.close().await.unwrap();
}

#[tokio::test]
async fn output_source_and_store_budgets_are_independent() {
    let mut limit = limits();
    limit.max_output_bytes = 7;
    let session = PtcSession::new(Arc::new(Fixture::default()), limit);
    let result = exec(&session, "text('你好世界'); text('more');").await;
    assert_eq!(result.output, ["你好", "m"]);
    assert!(result.truncated);
    assert_eq!(result.status, CellStatus::Completed);
    assert!(
        session
            .exec(&";".repeat(MAX_SOURCE_BYTES + 1), observe())
            .await
            .is_err()
    );
    let result = exec(&session, "store('huge', 'x'.repeat(1024*1024));").await;
    assert_eq!(result.status, CellStatus::Failed);
    assert!(result.error.unwrap().contains("store"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn runtime_memory_is_limited_and_session_recovers() {
    let mut limit = limits();
    limit.memory_mb = 4;
    let session = PtcSession::new(Arc::new(Fixture::default()), limit);
    let result = exec(
        &session,
        "const values=[]; for(let i=0;i<1000000;i++) values.push({i, data:'value'+i});",
    )
    .await;
    assert_eq!(result.status, CellStatus::Failed);
    assert_eq!(exec(&session, "text(42);").await.output, ["42"]);
    session.close().await.unwrap();
}

#[tokio::test]
async fn active_cell_cap_and_close_cancel_pending_cells() {
    let (session, _) = session();
    for _ in 0..MAX_ACTIVE_CELLS {
        let result = exec(&session, "yield_control(); await new Promise(()=>{});").await;
        assert_eq!(result.status, CellStatus::Running);
    }
    assert!(session.exec("text('excess');", observe()).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), session.close())
        .await
        .unwrap()
        .unwrap();
    assert!(session.exec("text('closed');", observe()).await.is_err());
}

#[tokio::test]
async fn tool_budget_is_shared_across_cells_in_a_user_turn() {
    let fixture = Arc::new(Fixture::default());
    let mut limits = limits();
    limits.max_tool_calls = 2;
    let session = PtcSession::new(fixture.clone(), limits);
    assert_eq!(
        exec(&session, "await tools.echo({});").await.status,
        CellStatus::Completed
    );
    assert_eq!(
        exec(&session, "await tools.echo({});").await.status,
        CellStatus::Completed
    );
    let rejected = exec(&session, "await tools.echo({});").await;
    assert_eq!(rejected.status, CellStatus::Failed);
    assert!(rejected.error.unwrap().contains("tool call budget"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    session.close().await.unwrap();
}
