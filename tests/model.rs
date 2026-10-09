//! Real HTTP + QuickJS path with a deterministic Responses fixture; no live model.
use anyhow::Result;
use aporto::{
    build::build,
    model::{ModelConfig, RunObserver, initial_input, model_tools, run_agent, run_agent_turn},
    types::{ToolBroker, ToolDefinition},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

#[path = "model/streaming.rs"]
mod streaming;

struct Echo {
    calls: AtomicUsize,
}

struct WorkspaceEcho(Echo);
#[async_trait]
impl ToolBroker for WorkspaceEcho {
    fn working_directory(&self) -> Option<&str> {
        Some("/projects/custom workspace")
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.0.definitions()
    }
    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> Result<Value> {
        self.0.call(name, args, cancel).await
    }
}
#[async_trait]
impl ToolBroker for Echo {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "echo".into(),
            description: "Echo input".into(),
            input_schema: json!({"type":"object"}),
            parallel: true,
        }]
    }
    async fn call(&self, name: &str, args: Value, _: CancellationToken) -> Result<Value> {
        assert_eq!(name, "echo");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(args)
    }
}

fn fixture_bundle() -> aporto::types::Bundle {
    fixture_bundle_with("", "")
}

fn fixture_bundle_with(model: &str, limits: &str) -> aporto::types::Bundle {
    let root = tempfile::tempdir().unwrap();
    let source = format!(
        r#"
[agent]
name = "model-test"
[model]
name = "fixture"
connection = "primary"
{model}
[runtime]
provider = "agentenv"
template = "fixture"
[[prompts]]
source = "prompt.md"
{limits}
"#
    );
    std::fs::write(root.path().join("Agentfile"), source).unwrap();
    std::fs::write(root.path().join("prompt.md"), "PACKAGED_PROMPT").unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "WORKSPACE_POISON").unwrap();
    build(root.path(), Path::new("Agentfile"), &BTreeMap::new()).unwrap()
}

async fn fixture(
    replies: Vec<Value>,
) -> (
    ModelConfig,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    fixture_path(replies, "/v1/responses").await
}

async fn fixture_path(
    replies: Vec<Value>,
    path: &str,
) -> (
    ModelConfig,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
    let expected_request_line = format!("POST {path} ");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let saved = requests.clone();
    let server = tokio::spawn(async move {
        for mut reply in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(i) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..end]);
            assert!(headers.starts_with(&expected_request_line));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse()
                .unwrap();
            while bytes.len() < end + length {
                let mut buffer = [0; 4096];
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let request: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
            for item in reply["output"].as_array_mut().unwrap() {
                if item["name"] == "wait" {
                    let mut args: Value =
                        serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap();
                    if args["cell_id"] == "$LAST_CELL_ID" {
                        let output: Value = serde_json::from_str(
                            request["input"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .rev()
                                .find(|item| item["type"] == "custom_tool_call_output")
                                .unwrap()["output"]
                                .as_str()
                                .unwrap(),
                        )
                        .unwrap();
                        args["cell_id"] = output["cell_id"].clone();
                        item["arguments"] = args.to_string().into();
                    }
                }
            }
            saved.lock().unwrap().push(request);
            let body = reply.to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    (
        ModelConfig {
            redacted_values: vec![],
            headers: Default::default(),
            endpoint,
            api_key: "fixture-key".into(),
            request_timeout: Duration::from_secs(5),
        },
        requests,
        server,
    )
}

fn reply(output: Value) -> Value {
    json!({"id":"r","status":"completed","output":output})
}
fn exec_call(id: &str, code: &str) -> Value {
    json!({"type":"custom_tool_call","name":"exec","call_id":id,"input":code})
}

#[derive(Default)]
struct Observer(Mutex<Vec<(String, Value)>>);
#[async_trait]
impl RunObserver for Observer {
    async fn emit(&self, kind: &str, data: Value) -> Result<()> {
        self.0.lock().unwrap().push((kind.into(), data));
        Ok(())
    }
}

struct FailToolCompletion;
#[async_trait]
impl RunObserver for FailToolCompletion {
    async fn emit(&self, kind: &str, _: Value) -> Result<()> {
        if kind == "tool.completed" {
            anyhow::bail!("transient persistence failure");
        }
        Ok(())
    }
}

struct BlockObserver(CancellationToken);
#[async_trait]
impl RunObserver for BlockObserver {
    async fn emit(&self, _: &str, _: Value) -> Result<()> {
        self.0.cancel();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_interrupts_a_blocked_progress_observer() {
    let bundle = fixture_bundle();
    let broker = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let (config, requests, server) = fixture(vec![]).await;
    let entered = CancellationToken::new();
    let cancel = CancellationToken::new();
    let run = run_agent_turn(
        &bundle,
        broker.clone(),
        config,
        "task",
        vec![],
        Arc::new(BlockObserver(entered.clone())),
        cancel.clone(),
    );
    tokio::pin!(run);
    tokio::select! {
        _ = entered.cancelled() => {}
        _ = &mut run => panic!("observer should block until cancellation"),
        _ = tokio::time::sleep(Duration::from_secs(1)) => panic!("observer was never entered"),
    }
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(1), run)
        .await
        .expect("cancellation must interrupt observer")
        .err()
        .expect("run must fail");
    assert!(error.to_string().contains("cancelled"));
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(broker.calls.load(Ordering::SeqCst), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn javascript_cannot_catch_a_progress_failure_and_repeat_side_effects() {
    let bundle = fixture_bundle();
    let broker = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let (config, requests, server) = fixture(vec![reply(json!([exec_call("event-failure",
        "try { text(await tools.echo({step:1})); } catch (_) {} try { text(await tools.echo({step:2})); } catch (_) {}"
    )]))]).await;
    let result = run_agent_turn(
        &bundle,
        broker.clone(),
        config,
        "test",
        vec![],
        Arc::new(FailToolCompletion),
        CancellationToken::new(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(broker.calls.load(Ordering::SeqCst), 1);
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.await.unwrap();
}

#[tokio::test]
async fn user_turn_continuation_preserves_history_and_public_transcript() {
    let bundle = fixture_bundle();
    let broker = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let observer = Arc::new(Observer::default());
    let (config, requests, server) = fixture(vec![
        reply(json!([exec_call("unique-1", "text(await tools.echo({private: 'secret-tool-result'}))")])),
        reply(json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"first answer"}]}])),
        reply(json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second answer"}]}])),
    ]).await;
    let first = run_agent_turn(
        &bundle,
        broker.clone(),
        config.clone(),
        "first request",
        vec![],
        observer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let second = run_agent_turn(
        &bundle,
        broker,
        config,
        "second request",
        first.history.clone(),
        observer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(second.run.answer, "second answer");
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    let last = requests[2]["input"].as_array().unwrap();
    assert_eq!(&last[..last.len() - 1], &first.history);
    assert_eq!(last.last().unwrap()["content"], "second request");
    assert_eq!(
        last.iter().filter(|item| item["role"] == "system").count(),
        1
    );
    let events = observer.0.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|(kind, data)| kind == "tool.completed" && data["success"] == true)
    );
    assert!(events.iter().any(|(kind, _)| kind == "model.started"));
    let serialized = serde_json::to_string(&*events).unwrap();
    assert!(serialized.contains("first answer"));
    assert!(serialized.contains("second answer"));
    for private in ["first request", "PACKAGED_PROMPT"] {
        assert!(!serialized.contains(private));
    }
}

#[tokio::test]
async fn responses_loop_runs_real_ptc_and_preserves_call_ids_and_reasoning() {
    let bundle = fixture_bundle();
    let broker = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let (config,requests,server)=fixture(vec![
        reply(json!([{"type":"reasoning","id":"reasoning-id","encrypted_content":"opaque-fixture"},exec_call("call-1","text(await tools.echo({value: 42}));")])),
        reply(json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Verified 42"}]}])),
    ]).await;
    let outcome = run_agent(
        &bundle,
        broker.clone(),
        config,
        "test task",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.answer, "Verified 42");
    assert_eq!(outcome.turns, 2);
    assert_eq!((outcome.exec_calls, outcome.wait_calls), (1, 0));
    assert_eq!(broker.calls.load(Ordering::SeqCst), 1);
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0]["parallel_tool_calls"], false);
    assert_eq!(requests[0]["store"], false);
    assert_eq!(requests[0]["tools"].as_array().unwrap().len(), 2);
    let history = requests[1]["input"].as_array().unwrap();
    assert!(
        history
            .iter()
            .any(|v| v["encrypted_content"] == "opaque-fixture")
    );
    let output = history
        .iter()
        .find(|v| v["type"] == "custom_tool_call_output")
        .unwrap();
    assert_eq!(output["call_id"], "call-1");
    assert!(output["output"].as_str().unwrap().contains("42"));
}

#[tokio::test]
async fn malformed_parallel_response_executes_nothing() {
    let bundle = fixture_bundle();
    let broker = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let (config, _, server) = fixture(vec![reply(json!([
        exec_call("one", "await tools.echo({});"),
        exec_call("two", "await tools.echo({});")
    ]))])
    .await;
    let error = run_agent(
        &bundle,
        broker.clone(),
        config,
        "task",
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("parallel_tool_calls"));
    assert_eq!(broker.calls.load(Ordering::SeqCst), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn unsupported_top_level_tools_are_rejected_before_local_side_effects() {
    let bundle = fixture_bundle();
    for output in [
        json!([{"type":"function_call","name":"echo","call_id":"direct","arguments":"{}"}]),
        json!([{"type":"function_call","name":"exec","call_id":"wrong-kind","arguments":"{}"}]),
        json!([exec_call("bad-status", "await tools.echo({})"), {"type":"web_search_call","status":"completed"}]),
        json!([{"type":"custom_tool_call","name":"exec","call_id":"incomplete","input":"await tools.echo({})","status":"in_progress"}]),
    ] {
        let broker = Arc::new(Echo {
            calls: AtomicUsize::new(0),
        });
        let (config, requests, server) = fixture(vec![reply(output)]).await;
        assert!(
            run_agent(
                &bundle,
                broker.clone(),
                config,
                "task",
                CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(broker.calls.load(Ordering::SeqCst), 0);
        assert_eq!(requests.lock().unwrap().len(), 1);
        server.await.unwrap();
    }
}

#[test]
fn prompt_assembly_has_no_workspace_discovery() {
    let input = serde_json::to_string(&initial_input(&fixture_bundle(), "task").unwrap()).unwrap();
    assert!(input.contains("PACKAGED_PROMPT"));
    assert!(!input.contains("WORKSPACE_POISON"));
    assert_eq!(model_tools()[0]["type"], "custom");
}

#[tokio::test]
async fn selected_workdir_is_given_to_the_model_before_the_current_task() {
    let (config, requests, server) = fixture(vec![reply(json!([{
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": "ok"}]
    }]))])
    .await;
    let broker = Arc::new(WorkspaceEcho(Echo {
        calls: AtomicUsize::new(0),
    }));
    run_agent(
        &fixture_bundle(),
        broker,
        config,
        "current task",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    let input = requests[0]["input"].as_array().unwrap();
    let workspace = &input[input.len() - 2];
    assert_eq!(workspace["role"], "developer");
    let context = workspace["content"].to_string();
    assert!(context.contains("/projects/custom workspace"));
    assert!(!requests[0].to_string().contains("WORKSPACE_POISON"));
    assert_eq!(input.last().unwrap()["role"], "user");
    assert_eq!(
        input.iter().filter(|item| item["role"] == "user").count(),
        1
    );
    assert!(
        input.last().unwrap()["content"]
            .to_string()
            .contains("current task")
    );
}

#[tokio::test]
async fn responses_wait_routes_incremental_output_with_function_call_id() {
    let bundle = fixture_bundle();
    let broker = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let (config, requests, server) = fixture(vec![
        reply(json!([exec_call("exec-id", "text('first'); await yield_control(); text('second');")])),
        reply(json!([{"type":"function_call","name":"wait","call_id":"wait-id",
            "arguments":"{\"cell_id\":\"$LAST_CELL_ID\",\"yield_time_ms\":1000}"}])),
        reply(json!([{"type":"message","content":[{"type":"output_text","text":"Both chunks received"}]}])),
    ]).await;
    let result = run_agent(&bundle, broker, config, "task", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.turns, 3);
    assert_eq!((result.exec_calls, result.wait_calls), (1, 1));
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    let history = requests[2]["input"].as_array().unwrap();
    let exec: Value = serde_json::from_str(
        history
            .iter()
            .find(|i| i["type"] == "custom_tool_call_output")
            .unwrap()["output"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let wait = history
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    let result: Value = serde_json::from_str(wait["output"].as_str().unwrap()).unwrap();
    assert_eq!(wait["call_id"], "wait-id");
    assert_eq!(exec["output"], json!(["first"]));
    assert_eq!(result["output"], json!(["second"]));
}

#[tokio::test]
async fn model_options_are_sent_only_when_configured_and_full_endpoint_is_preserved() {
    for options in [
        "",
        "max_output_tokens=1234\nreasoning={effort='high'}\ntext={verbosity='low'}",
    ] {
        let bundle = fixture_bundle_with(options, "");
        let (config, requests, server) = fixture_path(
            vec![reply(
                json!([{"type":"message","content":[{"type":"output_text","text":"done"}]}]),
            )],
            "/gateway/tenant/custom-response",
        )
        .await;
        run_agent(
            &bundle,
            Arc::new(Echo {
                calls: AtomicUsize::new(0),
            }),
            config,
            "task",
            CancellationToken::new(),
        )
        .await
        .unwrap();
        server.await.unwrap();
        let requests = requests.lock().unwrap();
        let request = &requests[0];
        assert_eq!(request["model"], "fixture");
        assert!(request.get("connection").is_none());
        assert!(request.get("protocol").is_none());
        assert_eq!(request["stream"], true);
        assert_eq!(
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["exec", "wait"]
        );
        assert_eq!(request["store"], false);
        assert_eq!(request["parallel_tool_calls"], false);
        if options.is_empty() {
            for name in ["max_output_tokens", "reasoning", "text"] {
                assert!(request.get(name).is_none());
            }
        } else {
            assert_eq!(request["max_output_tokens"], 1234);
            assert_eq!(request["reasoning"], json!({"effort":"high"}));
            assert_eq!(request["text"], json!({"verbosity":"low"}));
        }
    }
}

#[tokio::test]
async fn wait_max_tokens_controls_incremental_output_budget() {
    let bundle = fixture_bundle();
    let (config, requests, server) = fixture(vec![
        reply(json!([exec_call("one", "text('before'); yield_control(); text('abcdefghijk');")])),
        reply(json!([{"type":"function_call","name":"wait","call_id":"two","arguments":"{\"cell_id\":\"$LAST_CELL_ID\",\"max_tokens\":1,\"yield_time_ms\":1000}"}])),
        reply(json!([{"type":"message","content":[{"type":"output_text","text":"done"}]}])),
    ]).await;
    run_agent(
        &bundle,
        Arc::new(Echo {
            calls: AtomicUsize::new(0),
        }),
        config,
        "task",
        CancellationToken::new(),
    )
    .await
    .unwrap();
    server.await.unwrap();
    let requests = requests.lock().unwrap();
    let output = requests[2]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    let value: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
    assert_eq!(value["output"], json!(["abcd"]));
    assert_eq!(value["truncated"], true);
    let spec = model_tools();
    assert!(
        spec[1]["parameters"]["properties"]
            .get("max_tokens")
            .is_some()
    );
    assert!(
        spec[1]["parameters"]["properties"]
            .get("max_output_bytes")
            .is_none()
    );
}

#[tokio::test]
async fn invalid_exec_pragma_is_rejected_before_tool_dispatch() {
    for pragma in [
        r#"{"max_output_tokens":0}"#,
        r#"{"yield_time_ms":60001}"#,
        r#"{"max_output_bytes":100}"#,
    ] {
        let broker = Arc::new(Echo {
            calls: AtomicUsize::new(0),
        });
        let source = format!("// @exec: {pragma}\nawait tools.echo({{}});");
        let (config, requests, server) = fixture(vec![reply(json!([exec_call("one", &source)])), reply(json!([{"type":"message","content":[{"type":"output_text","text":"invalid settings"}]}]))]).await;
        run_agent(
            &fixture_bundle(),
            broker.clone(),
            config,
            "task",
            CancellationToken::new(),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(broker.calls.load(Ordering::SeqCst), 0);
        let requests = requests.lock().unwrap();
        let output = requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "custom_tool_call_output")
            .unwrap();
        let value: Value = serde_json::from_str(output["output"].as_str().unwrap()).unwrap();
        assert_eq!(value["status"], "failed");
    }
}

#[tokio::test]
async fn turn_deadline_interrupts_blocked_progress_without_sending_a_request() {
    let bundle = fixture_bundle_with("", "[limits.turn]\ntimeout_ms=25");
    let (config, requests, server) = fixture(vec![]).await;
    let entered = CancellationToken::new();
    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        run_agent_turn(
            &bundle,
            Arc::new(Echo {
                calls: AtomicUsize::new(0),
            }),
            config,
            "task",
            vec![],
            Arc::new(BlockObserver(entered.clone())),
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
    assert!(outcome.err().unwrap().to_string().contains("turn deadline"));
    assert!(entered.is_cancelled());
    assert!(requests.lock().unwrap().is_empty());
    server.await.unwrap();
}

struct PendingTool {
    active: AtomicUsize,
    started: AtomicUsize,
}
struct ActiveTool<'a>(&'a AtomicUsize);
impl Drop for ActiveTool<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl ToolBroker for PendingTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "pending".into(),
            description: "Wait until cancelled".into(),
            input_schema: json!({"type":"object"}),
            parallel: true,
        }]
    }
    async fn call(&self, _: &str, _: Value, cancel: CancellationToken) -> Result<Value> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = ActiveTool(&self.active);
        cancel.cancelled().await;
        anyhow::bail!("pending tool cancelled")
    }
}

#[tokio::test]
async fn turn_deadline_cancels_active_ptc_callbacks_before_returning() {
    let bundle = fixture_bundle_with("", "[limits.turn]\ntimeout_ms=250");
    let broker = Arc::new(PendingTool {
        active: AtomicUsize::new(0),
        started: AtomicUsize::new(0),
    });
    let (config, _, server) = fixture(vec![reply(json!([exec_call(
        "pending",
        "await tools.pending({});"
    )]))])
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        run_agent(
            &bundle,
            broker.clone(),
            config,
            "task",
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("turn deadline"));
    assert_eq!(broker.started.load(Ordering::SeqCst), 1);
    assert_eq!(broker.active.load(Ordering::SeqCst), 0);
    server.await.unwrap();
}
