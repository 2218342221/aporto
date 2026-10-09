#![cfg(feature = "test-fixtures")]
//! Real HTTP -> native JSON-RPC child contracts. No credentials or engine mocks
//! are silently substituted into the production server.
use aporto_core_client::{ClientOptions, CoreClient, CoreProcessConfig};
use aporto_server::{ServerConfig, router};
use reqwest::{Client, Method, Response};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "fixture-operator-token-at-least-32-bytes";

#[tokio::test]
async fn transcript_items_use_turn_scoped_core_route_and_immutable_cursor() {
    let h = Harness::start(128, 64, Duration::from_secs(2)).await;
    let turn: Value = h
        .request(Method::POST, "/v1/threads/thread-1/turns")
        .json(&json!({"input":"work","idempotency_key":"items"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let path = format!(
        "/v1/threads/thread-1/turns/{}/items",
        turn["id"].as_str().unwrap()
    );
    let response = h
        .request(Method::GET, &format!("{path}?after=0&limit=100"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let page: aporto_protocol::ItemListResult = response.json().await.unwrap();
    assert_eq!(
        page.items[0].content.text.as_deref(),
        Some("Inspecting workspace")
    );
    assert_eq!(page.items[0].ordinal, 3);
    assert_eq!(page.items[0].sequence, 4);
    assert_eq!(page.next_cursor, 3);
    let next: Value = h
        .request(Method::GET, &format!("{path}?after=3"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(next, json!({"items":[],"next_cursor":3,"has_more":false}));
    for query in [
        "after=-1",
        "after=18446744073709551616",
        "limit=no",
        "unknown=1",
    ] {
        assert_eq!(
            h.request(Method::GET, &format!("{path}?{query}"))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    assert_eq!(
        h.request(Method::GET, &format!("{path}?after=18446744073709551615"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        h.request(Method::GET, "/v1/threads/other/turns/turn-1/items")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        h.request(Method::GET, "/v1/threads/thread-1/turns/missing/items")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        h.http
            .get(format!("{}{path}", h.url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let stats: Value = h.core.call("fixture/stats", json!({})).await.unwrap();
    assert_eq!(stats["methods"]["item/list"], 5);
    h.close().await;
}

#[tokio::test]
async fn runtime_selection_and_owned_instance_listing_round_trip_through_http() {
    let h = Harness::start(128, 64, Duration::from_secs(2)).await;
    let agents: aporto_protocol::AgentListResult = h
        .request(Method::GET, "/v1/agents")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(agents.agents[0].runtime.default_image, "fixture:latest");
    assert_eq!(agents.agents[0].runtime.images.len(), 2);
    let result = h
        .request(Method::GET, "/v1/agents/fixture/instances?limit=1")
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    assert_eq!(result.headers()["cache-control"], "no-store");
    let page: aporto_protocol::RuntimeInstanceListResult = result.json().await.unwrap();
    assert_eq!(page.instances[0].sandbox_id, "sandbox-1");
    assert!(page.instances[0].busy);
    let next: aporto_protocol::RuntimeInstanceListResult = h
        .request(
            Method::GET,
            "/v1/agents/fixture/instances?cursor=instance-1",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(next.instances.is_empty());
    for query in [
        "limit=0",
        "limit=101",
        "limit=-1",
        "limit=4294967296",
        "unknown=1",
    ] {
        assert_eq!(
            h.request(
                Method::GET,
                &format!("/v1/agents/fixture/instances?{query}")
            )
            .send()
            .await
            .unwrap()
            .status(),
            400
        );
    }
    for path in [
        "/v1/agents/missing/instances",
        "/v1/agents/fixture/instances?cursor=missing",
    ] {
        assert_eq!(
            h.request(Method::GET, path).send().await.unwrap().status(),
            404
        );
    }
    assert_eq!(
        h.http
            .get(format!("{}/v1/agents/fixture/instances", h.url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let created = h.request(Method::POST,"/v1/threads").json(&json!({"agent_id":"fixture","runtime":{"mode":"new","image":"fixture:alternate"},"workdir":"/work/project"})).send().await.unwrap();
    assert_eq!(created.status(), 201);
    let thread: aporto_protocol::Thread = created.json().await.unwrap();
    assert_eq!(thread.runtime_image, "fixture:alternate");
    assert_eq!(thread.workdir, "/work/project");
    let reused: aporto_protocol::Thread = h
        .request(Method::POST, "/v1/threads")
        .json(&json!({"agent_id":"fixture","runtime":{"mode":"reuse","instance_id":"instance-1"}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reused.runtime_instance_id, "instance-1");
    assert_eq!(reused.sandbox_id.as_deref(), Some("sandbox-1"));
    for runtime in [
        json!({"mode":"reuse","instance_id":"instance-1","image":"fixture:alternate"}),
        json!({"mode":"attach","sandbox_id":"arbitrary"}),
        json!({"mode":"new","unknown":true}),
    ] {
        assert_eq!(
            h.request(Method::POST, "/v1/threads")
                .json(&json!({"agent_id":"fixture","runtime":runtime}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let stats: Value = h.core.call("fixture/stats", json!({})).await.unwrap();
    assert_eq!(stats["methods"]["runtime/instances"], 6);
    assert_eq!(stats["methods"]["thread/start"], 2);
    h.close().await;
}

struct Harness {
    url: String,
    core: CoreClient,
    http: Client,
    stop: CancellationToken,
    admissions: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Harness {
    async fn start(max_requests: usize, max_streams: usize, timeout: Duration) -> Self {
        let core = CoreClient::spawn(
            CoreProcessConfig {
                binary: PathBuf::from(env!("CARGO_BIN_EXE_aporto-server-core-fixture")),
                config: "unused".into(),
                state_dir: "unused".into(),
                releases_dir: None,
            },
            ClientOptions {
                request_timeout: Duration::from_secs(2),
                shutdown_timeout: Duration::from_millis(200),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let _: Value = core
            .call(
                "initialize",
                json!({"protocol_version":"1.0","client_name":"http-test"}),
            )
            .await
            .unwrap();
        let stop = CancellationToken::new();
        let admissions = CancellationToken::new();
        let mut config = ServerConfig::new(TOKEN.into(), vec!["http://localhost:5173".into()]);
        config.event_poll_interval = Duration::from_millis(10);
        config.request_timeout = timeout;
        config.max_requests = max_requests;
        config.max_streams = max_streams;
        let app = router(core.clone(), config, admissions.clone()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stopping.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            url,
            core,
            http: Client::builder().no_proxy().build().unwrap(),
            stop,
            admissions,
            task,
        }
    }
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(TOKEN)
    }
    async fn close(self) {
        self.admissions.cancel();
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .unwrap()
            .unwrap();
        self.core.shutdown().await.unwrap();
    }
}
async fn event(mut response: Response) -> String {
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        loop {
            let chunk = response.chunk().await.unwrap().unwrap();
            bytes.extend_from_slice(&chunk);
            if bytes.windows(2).any(|window| window == b"\n\n") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn bearer_origin_validation_and_body_errors_are_json_envelopes() {
    let h = Harness::start(128, 64, Duration::from_secs(2)).await;
    assert_eq!(
        h.http
            .get(format!("{}/healthz", h.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let unauth = h
        .http
        .get(format!("{}/v1/agents?token={TOKEN}", h.url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);
    assert_eq!(
        unauth.json::<Value>().await.unwrap()["error"]["code"],
        -32001
    );
    let wrong = h
        .http
        .get(format!("{}/v1/agents", h.url))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let duplicate = h
        .request(Method::GET, "/v1/agents")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 401);
    let lowercase = h
        .http
        .get(format!("{}/v1/agents", h.url))
        .header("Authorization", format!("bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(lowercase.status(), 200);
    let rejected = h
        .request(Method::GET, "/v1/agents")
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 403);
    let allowed = h
        .request(Method::GET, "/v1/agents")
        .header("Origin", "http://localhost:5173")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    let options = h
        .http
        .request(Method::OPTIONS, format!("{}/v1/threads", h.url))
        .header("Origin", "http://localhost:5173")
        .header("Access-Control-Request-Method", "POST")
        .header(
            "Access-Control-Request-Headers",
            "authorization,content-type",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(options.status(), 200);
    let malformed = h
        .request(Method::POST, "/v1/threads")
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
    assert!(
        malformed
            .json::<Value>()
            .await
            .unwrap()
            .get("error")
            .is_some()
    );
    let extra = h
        .request(Method::POST, "/v1/threads")
        .json(&json!({"agent_id":"fixture","bundle_path":"/etc/secrets"}))
        .send()
        .await
        .unwrap();
    assert_eq!(extra.status(), 400);
    let large = h
        .request(Method::POST, "/v1/threads")
        .header("content-type", "application/json")
        .body("x".repeat(1024 * 1024 + 1))
        .send()
        .await
        .unwrap();
    assert_eq!(large.status(), 413);
    assert!(large.json::<Value>().await.unwrap().get("error").is_some());
    h.close().await;
}

#[tokio::test]
async fn thread_turn_sse_replay_and_disconnect_are_rpc_only() {
    let h = Harness::start(128, 64, Duration::from_secs(2)).await;
    let created = h
        .request(Method::POST, "/v1/threads")
        .json(&json!({"agent_id":"fixture","title":"Test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    assert_eq!(created.json::<Value>().await.unwrap()["id"], "thread-1");
    let first = event(
        h.request(Method::GET, "/v1/threads/thread-1/events")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(first.contains("event: agent_event"));
    assert!(first.contains("id: 1"));
    let start = || {
        h.request(Method::POST, "/v1/threads/thread-1/turns")
            .json(&json!({"input":"hello","idempotency_key":"same-key"}))
    };
    let one = start().send().await.unwrap();
    assert_eq!(one.status(), 202);
    let one = one.json::<Value>().await.unwrap();
    let two = start().send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(one["id"], two["id"]);
    let replay = event(
        h.request(Method::GET, "/v1/threads/thread-1/events?after=1")
            .header("Last-Event-ID", "0")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(replay.contains("id: 2"));
    assert!(!replay.contains("id: 1"));
    let resumed = event(
        h.request(Method::GET, "/v1/threads/thread-1/events")
            .header("Last-Event-ID", "1")
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(resumed.contains("id: 2"));
    let page = h
        .request(Method::GET, "/v1/threads/thread-1/events/page?after=2")
        .header("Last-Event-ID", "invalid-but-overridden")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(page["events"].as_array().unwrap().is_empty());
    let bad = h
        .request(Method::GET, "/v1/threads/thread-1/events/page")
        .header("Last-Event-ID", "invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let read = h
        .request(Method::GET, "/v1/threads/thread-1?limit=20")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(read["turns"][0]["input"], "hello");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let stats: Value = h.core.call("fixture/stats", json!({})).await.unwrap();
    assert!(
        stats["methods"].get("turn/interrupt").is_none(),
        "SSE disconnect must not interrupt a turn"
    );
    let stopped = h
        .request(Method::POST, "/v1/threads/thread-1/turns/turn-1/interrupt")
        .send()
        .await
        .unwrap();
    assert_eq!(stopped.status(), 200);
    assert_eq!(
        stopped.json::<Value>().await.unwrap()["status"],
        "interrupted"
    );
    let stats: Value = h.core.call("fixture/stats", json!({})).await.unwrap();
    assert_eq!(stats["methods"]["turn/interrupt"], 1);
    h.close().await;
}

#[tokio::test]
async fn invalid_pages_fail_before_handshake_and_transient_poll_errors_remain_structured() {
    let h = Harness::start(128, 64, Duration::from_secs(2)).await;
    let _: Value = h
        .core
        .call("fixture/event-mode", json!({"mode":"skip"}))
        .await
        .unwrap();
    for path in [
        "/v1/threads/thread-1/events",
        "/v1/threads/thread-1/events/page",
    ] {
        let response = h.request(Method::GET, path).send().await.unwrap();
        assert_eq!(response.status(), 502);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            -32603
        );
    }
    let _: Value = h
        .core
        .call("fixture/event-mode", json!({"mode":""}))
        .await
        .unwrap();
    let held = h
        .request(Method::GET, "/v1/threads/thread-1/events?after=1")
        .send()
        .await
        .unwrap();
    let _: Value = h
        .core
        .call("fixture/event-mode", json!({"mode":"error"}))
        .await
        .unwrap();
    let message = event(held).await;
    assert!(message.contains("event: server_error"));
    assert!(message.contains("\"code\":-32010"));
    assert!(message.contains("fixture temporarily unavailable"));
    h.close().await;
}

#[tokio::test]
async fn shutdown_rejects_admissions_and_marks_readiness_before_core_exits() {
    let h = Harness::start(128, 64, Duration::from_secs(2)).await;
    h.admissions.cancel();
    for path in ["/v1/agents", "/v1/threads/thread-1/events", "/readinessz"] {
        assert_eq!(
            h.request(Method::GET, path).send().await.unwrap().status(),
            503
        );
    }
    assert_eq!(
        h.request(Method::GET, "/healthz")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(h.core.is_available());
    let stats: Value = h.core.call("fixture/stats", json!({})).await.unwrap();
    assert!(stats["methods"].get("agent/list").is_none());
    h.close().await;
}

#[tokio::test]
async fn slow_bodies_and_streams_have_separate_bounded_capacity() {
    let h = Harness::start(1, 1, Duration::from_millis(150)).await;
    let held = h
        .request(Method::GET, "/v1/threads/thread-1/events")
        .send()
        .await
        .unwrap();
    assert_eq!(held.status(), 200);
    let full = h
        .request(Method::GET, "/v1/threads/thread-1/events")
        .send()
        .await
        .unwrap();
    assert_eq!(full.status(), 429);
    // Holding an SSE permit does not use the ordinary request permit.
    assert_eq!(
        h.request(Method::GET, "/v1/agents")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut slow = TcpStream::connect(h.url.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    slow.write_all(format!("POST /v1/threads HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        h.request(Method::GET, "/v1/agents")
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    let mut response = [0; 4096];
    let size = tokio::time::timeout(Duration::from_secs(1), slow.read(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&response[..size]).contains("408"));
    drop(slow);
    drop(held);
    h.core.shutdown().await.unwrap();
    assert_eq!(
        h.http
            .get(format!("{}/readinessz", h.url))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    assert_eq!(
        h.http
            .get(format!("{}/healthz", h.url))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    h.close().await;
}
