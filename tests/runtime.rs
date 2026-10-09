//! Wire-contract tests use a local HTTP fixture, not a Firecracker microVM.
use aporto::{
    runtime::{RuntimeConfig, RuntimeMode, create_runtime, open_runtime},
    types::{ExecOptions, ProcessEvent, Runtime},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
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

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}
impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: value.to_string().into_bytes(),
        }
    }
    fn empty() -> Self {
        Self {
            status: 204,
            content_type: "application/proto",
            body: Vec::new(),
        }
    }
}

async fn server(
    handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
) -> (
    String,
    Arc<Mutex<Vec<Request>>>,
    tokio::task::JoinHandle<()>,
) {
    server_with_start_delay(handler, Duration::ZERO).await
}

async fn server_with_start_delay(
    handler: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    delay: Duration,
) -> (
    String,
    Arc<Mutex<Vec<Request>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests2 = requests.clone();
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let requests = requests2.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let end = loop {
                    let mut chunk = [0; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let header = std::str::from_utf8(&bytes[..end]).unwrap();
                let mut lines = header.lines();
                let mut start = lines.next().unwrap().split_whitespace();
                let method = start.next().unwrap().to_owned();
                let path = start.next().unwrap().to_owned();
                let headers = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                    .collect::<BTreeMap<_, _>>();
                let length = headers
                    .get("content-length")
                    .map(|length| length.parse::<usize>().unwrap())
                    .unwrap_or(0);
                while bytes.len() < end + length {
                    let mut chunk = [0; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let request = Request {
                    method,
                    path,
                    headers,
                    body: bytes[end..end + length].to_vec(),
                };
                let reply = handler(&request);
                let delay = if request.path == "/process.Process/Start" {
                    delay
                } else {
                    Duration::ZERO
                };
                requests.lock().unwrap().push(request);
                tokio::time::sleep(delay).await;
                let header = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reply.content_type,
                    reply.body.len()
                );
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                // Fragment frames across reads to exercise the incremental Connect decoder.
                for chunk in reply.body.chunks(3) {
                    if socket.write_all(chunk).await.is_err() {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            });
        }
    });
    (url, requests, task)
}

fn config(url: String, mode: RuntimeMode) -> RuntimeConfig {
    RuntimeConfig {
        mode,
        api_url: Some(url),
        sandbox_url: None,
        api_key: "test-control-key".into(),
        template: "test-template".into(),
        timeout_ms: 30_000,
    }
}

fn envelope(payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0];
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn command_stream() -> Vec<u8> {
    // Independently encoded process.proto StartResponse: PID 42, stdout "ok", exit 3.
    let mut bytes = envelope(&[10, 4, 10, 2, 8, 42]);
    bytes.extend(envelope(&[10, 6, 18, 4, 10, 2, b'o', b'k']));
    bytes.extend(envelope(&[10, 4, 26, 2, 8, 6]));
    bytes
}

#[tokio::test]
async fn failed_pause_and_delete_remain_retryable_without_reopening_data_operations() {
    let pauses = Arc::new(AtomicUsize::new(0));
    let deletes = Arc::new(AtomicUsize::new(0));
    let p = pauses.clone();
    let d = deletes.clone();
    let (url, _, server) = server(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/v2/sandboxes") => {
            Reply::json(json!({"sandboxID":"retained","envdAccessToken":"token"}))
        }
        ("POST", "/sandboxes/retained/pause") if p.fetch_add(1, Ordering::SeqCst) == 0 => Reply {
            status: 503,
            content_type: "application/json",
            body: vec![],
        },
        ("DELETE", "/sandboxes/retained") if d.fetch_add(1, Ordering::SeqCst) == 0 => Reply {
            status: 503,
            content_type: "application/json",
            body: vec![],
        },
        ("POST", "/sandboxes/retained/pause") | ("DELETE", "/sandboxes/retained") => Reply::empty(),
        _ => panic!("unexpected request {r:?}"),
    })
    .await;
    let runtime = open_runtime(config(url, RuntimeMode::Local), None)
        .await
        .unwrap();
    assert!(runtime.pause().await.is_err());
    assert!(runtime.read_file("/workspace/file").await.is_err());
    runtime.pause().await.unwrap();
    runtime.pause().await.unwrap();
    assert_eq!(pauses.load(Ordering::SeqCst), 2);
    assert!(runtime.close().await.is_err()); // A paused runtime may still be deleted.
    assert!(runtime.read_file("/workspace/file").await.is_err());
    runtime.close().await.unwrap();
    runtime.close().await.unwrap();
    assert_eq!(deletes.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn failed_teardown_does_not_skip_dropped_process_cleanup() {
    for pause in [false, true] {
        let (url, requests, task) = server(|request| match request.path.as_str() {
            "/v2/sandboxes" => {
                Reply::json(json!({"sandboxID":"cleanup","envdAccessToken":"token"}))
            }
            "/process.Process/Start" => Reply {
                status: 200,
                content_type: "application/connect+proto",
                body: envelope(&[10, 4, 10, 2, 8, 42]),
            },
            "/sandboxes/cleanup" | "/sandboxes/cleanup/pause" => Reply {
                status: 503,
                content_type: "application/json",
                body: vec![],
            },
            "/process.Process/SendSignal" => Reply::empty(),
            _ => panic!("unexpected request"),
        })
        .await;
        let runtime = open_runtime(config(url, RuntimeMode::Local), None)
            .await
            .unwrap();
        let process = runtime
            .start_process(&["fixture".into()], ExecOptions::default())
            .await
            .unwrap();
        let result = if pause {
            runtime.pause().await
        } else {
            runtime.close().await
        };
        assert!(result.is_err());
        drop(process);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| request.path.ends_with("/SendSignal"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("failed teardown must retain process cleanup");
        task.abort();
    }
}

#[tokio::test]
async fn command_deadline_includes_start_handshake_and_cancels_by_tag() {
    let (url, requests, server) = server_with_start_delay(
        |r| match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/v2/sandboxes") => {
                Reply::json(json!({"sandboxID":"slow-start","envdAccessToken":"token"}))
            }
            ("POST", "/process.Process/Start") => Reply {
                status: 200,
                content_type: "application/connect+proto",
                body: command_stream(),
            },
            ("POST", "/process.Process/SendSignal") | ("DELETE", "/sandboxes/slow-start") => {
                Reply::empty()
            }
            _ => panic!("unexpected request {r:?}"),
        },
        Duration::from_millis(200),
    )
    .await;
    let runtime = create_runtime(config(url, RuntimeMode::Local))
        .await
        .unwrap();
    let result = runtime
        .exec(
            "example",
            ExecOptions {
                timeout_ms: Some(25),
                ..Default::default()
            },
        )
        .await;
    assert!(
        result.is_err(),
        "a short command deadline must not wait through a slow start"
    );
    for _ in 0..100 {
        if requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/SendSignal"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.path.ends_with("/SendSignal")
                && r.body.windows(b"aporto-".len()).any(|w| w == b"aporto-")),
        "start timeout must terminate the unknown PID by tag"
    );
    runtime.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn reconnect_pauses_same_workspace_without_deleting_or_creating() {
    let (url, requests, task) = server(|r| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/v2/sandboxes/retained-id/connect") => {
            Reply::json(json!({"sandboxID":"retained-id","envdAccessToken":"fresh-token"}))
        }
        ("GET", path) if path.starts_with("/files?") => Reply {
            status: 200,
            content_type: "text/plain",
            body: b"previous turn content".to_vec(),
        },
        ("POST", "/sandboxes/retained-id/pause") => Reply::empty(),
        _ => panic!("unexpected request {r:?}"),
    })
    .await;
    let runtime = open_runtime(config(url, RuntimeMode::Remote), Some("retained-id"))
        .await
        .unwrap();
    assert_eq!(runtime.id(), "retained-id");
    assert_eq!(
        runtime.read_file("/workspace/old.txt").await.unwrap(),
        b"previous turn content"
    );
    runtime.pause().await.unwrap();
    runtime.pause().await.unwrap();
    assert!(runtime.read_file("/workspace/old.txt").await.is_err());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"timeout":30})
    );
    assert_eq!(requests[1].headers["x-access-token"], "fresh-token");
    assert!(!requests.iter().any(|r| r.method == "DELETE"));
    task.abort();
}

#[tokio::test]
async fn missing_sandbox_never_falls_back_to_an_empty_replacement() {
    let (url, requests, task) = server(|r| {
        assert_eq!(r.path, "/v2/sandboxes/missing/connect");
        Reply {
            status: 404,
            content_type: "application/json",
            body: br#"{"error":"missing"}"#.to_vec(),
        }
    })
    .await;
    assert!(
        open_runtime(config(url, RuntimeMode::Local), Some("missing"))
            .await
            .is_err()
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn real_http_adapter_uses_control_and_data_credentials_separately() {
    let (url, requests, task) =
        server(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/v2/sandboxes") => Reply::json(
                    json!({"sandboxID":"fixture-sandbox","envdAccessToken":"test-envd-key"}),
                ),
                ("POST", "/process.Process/Start") => Reply {
                    status: 200,
                    content_type: "application/connect+proto",
                    body: command_stream(),
                },
                ("GET", path) if path.starts_with("/files?") => Reply {
                    status: 200,
                    content_type: "text/plain",
                    body: b"fixture file".to_vec(),
                },
                ("POST", path) if path.starts_with("/files?") => Reply::json(json!({})),
                ("DELETE", "/sandboxes/fixture-sandbox") => Reply::empty(),
                _ => panic!("unexpected request {request:?}"),
            },
        )
        .await;
    let runtime = create_runtime(config(url, RuntimeMode::Local))
        .await
        .unwrap();
    let output = runtime
        .exec("printf ok; exit 3", ExecOptions::default())
        .await
        .unwrap();
    assert_eq!((output.stdout.as_str(), output.exit_code), ("ok", 3));
    assert_eq!(
        runtime.read_file("/workspace/test file").await.unwrap(),
        b"fixture file"
    );
    runtime
        .write_file("/workspace/new.txt", b"written content")
        .await
        .unwrap();
    runtime.close().await.unwrap();
    runtime.close().await.unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|r| r.method == "DELETE").count(), 1);
    let create = &requests[0];
    let body: Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(body["templateID"], "test-template");
    assert_eq!(body["network"]["allowPublicTraffic"], false);
    assert_eq!(body["timeout"], 30);
    assert!(body.get("mcp").is_none());
    let start = requests
        .iter()
        .find(|request| request.path.ends_with("/Start"))
        .unwrap();
    assert_eq!(start.headers["connect-timeout-ms"], "30000");
    assert!(
        start.body.ends_with(&[32, 0]),
        "one-shot exec closes stdin (StartRequest field 4)"
    );
    for request in requests.iter() {
        if request.path.starts_with("/files") || request.path.starts_with("/process.") {
            assert_eq!(request.headers["x-access-token"], "test-envd-key");
            assert_eq!(request.headers["x-agentenv-sandbox-id"], "fixture-sandbox");
            assert_eq!(request.headers["x-agentenv-target-port"], "49983");
            assert!(!request.headers.contains_key("x-api-key"));
        } else {
            assert_eq!(request.headers["x-api-key"], "test-control-key");
            assert!(!request.headers.contains_key("x-access-token"));
        }
    }
    let upload = requests
        .iter()
        .find(|r| r.method == "POST" && r.path.starts_with("/files"))
        .unwrap();
    let body = String::from_utf8_lossy(&upload.body);
    assert!(body.contains("name=\"file\""));
    assert!(body.contains("written content"));
    task.abort();
}

#[tokio::test]
async fn persistent_guest_process_sends_stdin_and_signal_with_protobuf() {
    let (url, requests, task) = server(|request| match request.path.as_str() {
        "/v2/sandboxes" => {
            Reply::json(json!({"sandboxID":"fixture-sandbox","envdAccessToken":"test-envd-key"}))
        }
        "/process.Process/Start" => Reply {
            status: 200,
            content_type: "application/connect+proto",
            body: command_stream(),
        },
        "/process.Process/SendInput"
        | "/process.Process/SendSignal"
        | "/sandboxes/fixture-sandbox" => Reply::empty(),
        _ => panic!("unexpected request"),
    })
    .await;
    let runtime = create_runtime(config(url, RuntimeMode::Remote))
        .await
        .unwrap();
    let mut process = runtime
        .start_process(
            &["python3".into(), "server.py".into()],
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(process.id(), 42);
    process.send(b"").await.unwrap();
    process.send(b"rpc\n").await.unwrap();
    assert!(
        matches!(process.next().await.unwrap(), Some(ProcessEvent::Stdout(bytes)) if bytes == b"ok")
    );
    process.close().await.unwrap();
    runtime.close().await.unwrap();
    let requests = requests.lock().unwrap();
    let start = requests
        .iter()
        .find(|request| request.path.ends_with("/Start"))
        .unwrap();
    assert!(!start.headers.contains_key("connect-timeout-ms"));
    assert!(
        start.body.ends_with(&[32, 1]),
        "persistent process keeps stdin open"
    );
    let input = requests
        .iter()
        .find(|r| r.path.ends_with("/SendInput"))
        .unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.ends_with("/SendInput"))
            .count(),
        1,
        "empty stdin writes are no-ops"
    );
    assert_eq!(
        input.body,
        [10, 2, 8, 42, 18, 6, 10, 4, b'r', b'p', b'c', b'\n']
    );
    let kill = requests
        .iter()
        .find(|r| r.path.ends_with("/SendSignal"))
        .unwrap();
    assert_eq!(kill.body, [10, 2, 8, 42, 16, 9]);
    task.abort();
}

#[tokio::test]
async fn invalid_modes_fail_before_network_without_host_fallback() {
    let mut remote = config("http://127.0.0.1:1".into(), RuntimeMode::Remote);
    remote.api_url = None;
    assert!(
        create_runtime(remote)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("explicit")
    );
    let local = config("https://runtime.example.com".into(), RuntimeMode::Local);
    assert!(
        create_runtime(local)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("loopback")
    );
    let mut config = config("http://127.0.0.1:1".into(), RuntimeMode::Local);
    config.sandbox_url = Some("https://runtime.example.com".into());
    assert!(
        create_runtime(config)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("loopback")
    );
}

#[tokio::test]
async fn missing_envd_token_deletes_the_created_sandbox() {
    let (url, requests, task) =
        server(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/v2/sandboxes") => Reply::json(json!({"sandboxID":"fixture-sandbox"})),
                ("DELETE", "/sandboxes/fixture-sandbox") => Reply::empty(),
                _ => panic!("unexpected request"),
            },
        )
        .await;
    assert!(
        create_runtime(config(url, RuntimeMode::Local))
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("secure envd")
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.method == "DELETE")
    );
    task.abort();
}

#[tokio::test]
async fn dropping_process_on_temporary_runtime_sends_kill_on_control_runtime() {
    let (url, requests, task) = server(|request| match request.path.as_str() {
        "/v2/sandboxes" => {
            Reply::json(json!({"sandboxID":"fixture-sandbox","envdAccessToken":"test-envd-key"}))
        }
        "/process.Process/Start" => Reply {
            status: 200,
            content_type: "application/connect+proto",
            body: envelope(&[10, 4, 10, 2, 8, 42]),
        },
        "/process.Process/SendSignal" | "/sandboxes/fixture-sandbox" => Reply::empty(),
        _ => panic!("unexpected request"),
    })
    .await;
    let runtime = create_runtime(config(url, RuntimeMode::Local))
        .await
        .unwrap();
    let temporary_runtime_handle = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let temporary = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        temporary.block_on(async {
            let process = temporary_runtime_handle
                .start_process(&["fixture".into()], ExecOptions::default())
                .await
                .unwrap();
            drop(process);
        });
        // Immediately destroying this executor must not discard the queued kill.
        drop(temporary);
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.path.ends_with("/SendSignal"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    runtime.close().await.unwrap();
    task.abort();
}
