//! Real HTTP transport contract against a fixture; this is not a live MCP deployment.
use anyhow::{Result, bail};
use aporto::{broker::create_broker, types::*};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

struct NoProcesses;
#[async_trait]
impl Runtime for NoProcesses {
    fn id(&self) -> &str {
        "explicit-fake-runtime"
    }
    async fn exec(&self, _: &str, _: ExecOptions) -> Result<CommandResult> {
        Ok(CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        })
    }
    async fn start_process(&self, _: &[String], _: ExecOptions) -> Result<Box<dyn RuntimeProcess>> {
        bail!("HTTP MCP must not start any process")
    }
    async fn read_file(&self, _: &str) -> Result<Vec<u8>> {
        bail!("unexpected file read")
    }
    async fn write_file(&self, _: &str, _: &[u8]) -> Result<()> {
        bail!("unexpected file upload")
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[tokio::test]
async fn streamable_http_handles_sse_recovers_from_rpc_error_and_retries_failed_cleanup() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::<Request>::new()));
    let captures = captured.clone();
    let server = tokio::spawn(async move {
        let mut deletes = 0;
        for _ in 0..7 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut buffer = [0u8; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let mut lines = headers.lines();
            let request_line = lines.next().unwrap();
            assert!(request_line.contains(" /mcp "));
            let method = request_line.split_whitespace().next().unwrap().to_owned();
            let headers = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                .collect::<BTreeMap<_, _>>();
            let length = headers
                .get("content-length")
                .map(|v| v.parse::<usize>().unwrap())
                .unwrap_or(0);
            while bytes.len() < end + length {
                let mut buffer = [0u8; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let body = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice::<Value>(&bytes[end..end + length]).unwrap()
            };
            let (status, content_type, extra_headers, response_body) = if method == "DELETE" {
                deletes += 1;
                (
                    if deletes == 1 { 503 } else { 204 },
                    "application/json",
                    "",
                    String::new(),
                )
            } else {
                match body["method"].as_str().unwrap() {
                    "initialize" => (200,"application/json","MCP-Session-Id: fixture-session\r\n",json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}).to_string()),
                    "notifications/initialized" => (202,"application/json","",String::new()),
                    "tools/list" => {
                        let result = json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}});
                        (200,"text/event-stream","",format!("event: message\r\ndata: {result}\r\n\r\n"))
                    }
                    "tools/call" => {
                        let result = if body["params"]["arguments"]["text"] == "reject" {
                            json!({"jsonrpc":"2.0","id":body["id"],"error":{"code":-32602,"message":"fixture argument rejected"}})
                        } else {
                            json!({"jsonrpc":"2.0","id":body["id"],"result":{"content":[{"type":"text","text":body["params"]["arguments"]["text"]}]}})
                        };
                        (200,"text/event-stream","",format!("data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{{}}}}\r\n\r\ndata: {result}\n\n"))
                    }
                    _ => panic!("unexpected MCP request: {body}"),
                }
            };
            captures.lock().unwrap().push(Request {
                method,
                headers,
                body,
            });
            let response_header = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
                response_body.len()
            );
            socket.write_all(response_header.as_bytes()).await.unwrap();
            for chunk in response_body.as_bytes().chunks(7) {
                socket.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        }
    });
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Agentfile"),
        format!(
            r#"
[agent]
name = "http-fixture"
[model]
name = "model"
connection = "primary"
[runtime]
provider = "agentenv"
template = "template"
[[mcp]]
name = "fixture"
url = {}
headers = {{ Authorization = {{ secret = "HTTP_TOKEN" }} }}
include_tools = ["echo"]
"#,
            serde_json::to_string(&endpoint).unwrap()
        ),
    )
    .unwrap();
    let bundle = aporto::build::build(
        root.path(),
        std::path::Path::new("Agentfile"),
        &BTreeMap::new(),
    )
    .unwrap();
    let broker = create_broker(
        bundle,
        Arc::new(NoProcesses),
        BTreeMap::from([
            ("HTTP_TOKEN".into(), "Bearer fixture-token".into()),
            ("UNRELATED".into(), "not-forwarded".into()),
        ]),
    )
    .await
    .unwrap();
    let rejected = broker
        .call(
            "mcp__fixture__echo",
            json!({"text":"reject"}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(rejected.to_string().contains("JSON-RPC error -32602"));
    let result = broker
        .call(
            "mcp__fixture__echo",
            json!({"text":"remote"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "remote");
    assert!(broker.close().await.is_err());
    broker.close().await.unwrap();
    broker.close().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 7);
    assert_eq!(captured[0].body["method"], "initialize");
    assert!(!captured[0].headers.contains_key("mcp-session-id"));
    assert_eq!(captured[1].body["method"], "notifications/initialized");
    for request in captured.iter() {
        assert_eq!(request.headers["authorization"], "Bearer fixture-token");
        assert_eq!(request.headers["mcp-protocol-version"], "2025-03-26");
        assert!(
            !request
                .headers
                .values()
                .any(|value| value.contains("not-forwarded"))
        );
    }
    for request in &captured[1..] {
        assert_eq!(request.headers["mcp-session-id"], "fixture-session");
    }
    assert_eq!(captured[3].body["params"]["arguments"]["text"], "reject");
    assert_eq!(captured[4].body["params"]["arguments"]["text"], "remote");
    assert_eq!(captured[5].method, "DELETE");
    assert_eq!(captured[6].method, "DELETE");
}
