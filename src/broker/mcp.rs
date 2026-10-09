//! MCP session lifecycle and stdio / Streamable HTTP transports.
use super::relative_path;
use crate::types::{ExecOptions, McpSpec, ProcessEvent, Runtime, RuntimeProcess, ValueRef};
use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use reqwest::{
    Client, Url,
    header::{HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
mod sse;

const MCP_MAX_BYTES: usize = 8 * 1024 * 1024;

fn secret_values(
    values: &BTreeMap<String, ValueRef>,
    declared: &[String],
    secrets: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|(name, value)| {
            let value = if let ValueRef::Secret { secret } = value {
                ensure!(
                    declared.iter().any(|name| name == secret),
                    "MCP references undeclared secret {secret}"
                );
                secrets
                    .get(secret)
                    .with_context(|| format!("missing secret {secret}"))?
                    .clone()
            } else if let ValueRef::Literal(value) = value {
                value.clone()
            } else {
                unreachable!()
            };
            Ok((name.clone(), value))
        })
        .collect()
}

enum McpTransport {
    Stdio {
        process: Box<dyn RuntimeProcess>,
        buffer: Vec<u8>,
    },
    Http {
        client: Client,
        url: Url,
        session: Option<String>,
    },
}

pub(super) struct McpClient {
    transport: McpTransport,
    next_id: u64,
    protocol: String,
    closed: bool,
    cleanup_complete: bool,
}

impl McpClient {
    pub(super) fn protocol(&self) -> &str {
        &self.protocol
    }

    pub(super) async fn connect(
        spec: &McpSpec,
        runtime: Arc<dyn Runtime>,
        declared: &[String],
        secrets: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let transport = match spec.transport.as_str() {
            "stdio" => {
                let env = secret_values(&spec.env, declared, secrets)?;
                let cwd = match spec.path.as_deref() {
                    Some(path) => format!("/opt/agent/{}", relative_path(path)?),
                    None => "/opt/agent".into(),
                };
                let argv = spec
                    .command
                    .as_ref()
                    .context("stdio MCP requires command argv")?;
                let process = runtime
                    .start_process(
                        argv,
                        ExecOptions {
                            cwd: Some(cwd),
                            env,
                            ..Default::default()
                        },
                    )
                    .await?;
                McpTransport::Stdio {
                    process,
                    buffer: Vec::new(),
                }
            }
            "http" => {
                let url = Url::parse(spec.url.as_deref().context("HTTP MCP requires URL")?)?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "invalid MCP HTTP endpoint"
                );
                let mut headers = HeaderMap::new();
                for (key, value) in secret_values(&spec.headers, declared, secrets)? {
                    let mut header = HeaderValue::from_str(&value).context("invalid MCP header")?;
                    header.set_sensitive(true);
                    headers.insert(
                        reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
                        header,
                    );
                }
                let mut client = Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .default_headers(headers);
                if url.host_str().is_some_and(|host| {
                    host == "localhost"
                        || host
                            .trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                }) {
                    client = client.no_proxy();
                }
                let client = client.build()?;
                McpTransport::Http {
                    client,
                    url,
                    session: None,
                }
            }
            _ => bail!("only stdio and Streamable HTTP MCP transports are supported"),
        };
        Ok(Self {
            transport,
            next_id: 1,
            protocol: "2025-03-26".into(),
            closed: false,
            cleanup_complete: false,
        })
    }

    pub(super) async fn initialize(&mut self) -> Result<()> {
        let initialized = self.request("initialize", json!({"protocolVersion":self.protocol,"capabilities":{},"clientInfo":{"name":"aporto","version":env!("CARGO_PKG_VERSION")}}), CancellationToken::new()).await?;
        let protocol = initialized
            .get("protocolVersion")
            .and_then(Value::as_str)
            .context("MCP initialize lacks protocolVersion")?;
        ensure!(
            matches!(protocol, "2024-11-05" | "2025-03-26" | "2025-06-18"),
            "unsupported MCP protocol {protocol}"
        );
        self.protocol = protocol.into();
        self.notify(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await
    }

    pub(super) async fn tools(&mut self) -> Result<BTreeMap<String, Value>> {
        let mut result = BTreeMap::new();
        let mut cursor: Option<String> = None;
        let mut cursors = BTreeSet::new();
        let mut catalog_bytes = 0usize;
        for _ in 0..100 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
            let page = self
                .request("tools/list", params, CancellationToken::new())
                .await?;
            for tool in page
                .get("tools")
                .and_then(Value::as_array)
                .context("MCP tools/list lacks tools array")?
            {
                catalog_bytes += serde_json::to_vec(tool)?.len();
                ensure!(
                    catalog_bytes <= MCP_MAX_BYTES,
                    "MCP tool catalog exceeds 8 MiB across pages"
                );
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .context("MCP tool lacks name")?;
                ensure!(
                    result.insert(name.to_owned(), tool.clone()).is_none(),
                    "duplicate MCP tool {name}"
                );
                ensure!(result.len() <= 10000, "too many MCP tools");
            }
            cursor = match page.get("nextCursor") {
                None => None,
                Some(Value::String(cursor)) => Some(cursor.clone()),
                Some(_) => bail!("MCP nextCursor must be text"),
            };
            match &cursor {
                None => return Ok(result),
                Some(cursor) => {
                    ensure!(cursors.insert(cursor.clone()), "MCP repeated tools cursor")
                }
            }
        }
        bail!("MCP tool pagination limit exceeded")
    }

    async fn notify(&mut self, message: Value) -> Result<()> {
        match &mut self.transport {
            McpTransport::Stdio { process, .. } => {
                process
                    .send(format!("{}\n", serde_json::to_string(&message)?).as_bytes())
                    .await
            }
            McpTransport::Http {
                client,
                url,
                session,
            } => {
                let mut request = client
                    .post(url.clone())
                    .header("Accept", "application/json, text/event-stream")
                    .header("MCP-Protocol-Version", &self.protocol)
                    .json(&message);
                if let Some(session) = session {
                    request = request.header("MCP-Session-Id", session.as_str());
                }
                let response = request.timeout(Duration::from_secs(10)).send().await?;
                ensure!(
                    response.status().is_success(),
                    "MCP notification HTTP {}",
                    response.status()
                );
                Ok(())
            }
        }
    }

    pub(super) async fn request(
        &mut self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(!self.closed, "MCP session is closed");
        ensure!(!cancel.is_cancelled(), "MCP call cancelled before dispatch");
        // A dropped request may already have sent bytes and caused side effects.
        // Poison the session until a complete matching reply is received, so a
        // cancelled future cannot leave stale replies for the next request.
        self.closed = true;
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let protocol = self.protocol.clone();
        let operation = async {
            match &mut self.transport {
                McpTransport::Stdio { process, buffer } => {
                    process
                        .send(format!("{}\n", serde_json::to_string(&message)?).as_bytes())
                        .await?;
                    let mut scan_from = 0;
                    loop {
                        while let Some(offset) =
                            buffer[scan_from..].iter().position(|&byte| byte == b'\n')
                        {
                            let end = scan_from + offset;
                            let line = buffer.drain(..=end).collect::<Vec<_>>();
                            scan_from = 0;
                            if line.iter().all(u8::is_ascii_whitespace) {
                                continue;
                            }
                            let message: Value = serde_json::from_slice(&line).context(
                                "MCP stdout must contain newline-delimited JSON-RPC only",
                            )?;
                            if let Some(response) = rpc_response(&message, id)? {
                                return Ok(response);
                            }
                            if let Some(request_id) = message.get("id") {
                                let response = if message.get("method").and_then(Value::as_str)
                                    == Some("ping")
                                {
                                    json!({"jsonrpc":"2.0","id":request_id,"result":{}})
                                } else {
                                    json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"Aporto does not expose client capabilities"}})
                                };
                                process.send(format!("{response}\n").as_bytes()).await?;
                            }
                        }
                        // Resume at the unexamined suffix when a frame arrives in tiny
                        // fragments; rescanning the whole prefix is quadratic in size.
                        scan_from = buffer.len();
                        match process.next().await? {
                            Some(ProcessEvent::Stdout(data)) => {
                                ensure!(
                                    buffer.len() + data.len() <= MCP_MAX_BYTES,
                                    "MCP stdout frame exceeds 8 MiB"
                                );
                                buffer.extend(data);
                            }
                            Some(ProcessEvent::Stderr(_)) => {} // Avoid leaking credential-bearing server logs.
                            Some(ProcessEvent::Exit(code)) => {
                                bail!("MCP process exited with {code}")
                            }
                            None => bail!("MCP process disconnected"),
                        }
                    }
                }
                McpTransport::Http {
                    client,
                    url,
                    session,
                } => {
                    let mut request = client
                        .post(url.clone())
                        .header("Accept", "application/json, text/event-stream")
                        .header("MCP-Protocol-Version", protocol)
                        .json(&message);
                    if let Some(session) = session.as_ref() {
                        request = request.header("MCP-Session-Id", session);
                    }
                    let response = request.send().await?;
                    ensure!(
                        response.status().is_success(),
                        "MCP HTTP {}",
                        response.status()
                    );
                    if let Some(value) = response.headers().get("MCP-Session-Id") {
                        let value = value.to_str()?.to_owned();
                        ensure!(
                            !value.is_empty()
                                && value.len() <= 1024
                                && value.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                            "invalid MCP session ID"
                        );
                        if let Some(existing) = session.as_ref() {
                            ensure!(existing == &value, "MCP session ID changed");
                        }
                        *session = Some(value);
                    }
                    let sse = response
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| {
                            v.split(';')
                                .next()
                                .is_some_and(|t| t.trim().eq_ignore_ascii_case("text/event-stream"))
                        });
                    let mut stream = response.bytes_stream();
                    let mut bytes = Vec::new();
                    let mut events = sse::Decoder::default();
                    let mut total = 0usize;
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk?;
                        total += chunk.len();
                        ensure!(total <= MCP_MAX_BYTES, "MCP HTTP response exceeds 8 MiB");
                        if sse {
                            for data in events.feed(&chunk) {
                                let message: Value = serde_json::from_slice(&data)?;
                                if let Some(response) = rpc_response(&message, id)? {
                                    return Ok(response);
                                }
                                ensure!(
                                    message.get("id").is_none(),
                                    "HTTP MCP server-initiated requests are unsupported"
                                );
                            }
                        } else {
                            bytes.extend_from_slice(&chunk);
                        }
                    }
                    ensure!(!sse, "MCP SSE response ended without a result");
                    let message: Value = serde_json::from_slice(&bytes)?;
                    rpc_response(&message, id)?
                        .context("MCP HTTP response did not match the request")
                }
            }
        };
        let result = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(60), operation) => result.context("MCP request timed out")?,
            () = cancel.cancelled() => bail!("MCP call cancelled; completion is unknown"),
        };
        match result {
            Ok(reply) => {
                // A matching JSON-RPC error is a completed exchange, too. The
                // caller can correct the request without replacing the session.
                self.closed = false;
                reply.into_result()
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn is_usable(&self) -> bool {
        !self.closed
    }

    pub(super) async fn close(&mut self) -> Result<()> {
        if self.cleanup_complete {
            return Ok(());
        }
        self.closed = true;
        let result = match &mut self.transport {
            McpTransport::Stdio { process, .. } => process.close().await,
            McpTransport::Http {
                client,
                url,
                session: Some(session),
            } => {
                let response = client
                    .delete(url.clone())
                    .header("MCP-Session-Id", session.as_str())
                    .header("MCP-Protocol-Version", &self.protocol)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await?;
                ensure!(
                    response.status().is_success()
                        || matches!(response.status().as_u16(), 404 | 405),
                    "MCP session cleanup HTTP {}",
                    response.status()
                );
                Ok(())
            }
            _ => Ok(()),
        };
        if result.is_ok() {
            self.cleanup_complete = true;
        }
        result
    }
}

enum RpcReply {
    Success(Value),
    ServerError(i64),
}

impl RpcReply {
    fn into_result(self) -> Result<Value> {
        match self {
            Self::Success(value) => Ok(value),
            // Arbitrary server error messages may contain request secrets.
            Self::ServerError(code) => bail!("MCP server returned JSON-RPC error {code}"),
        }
    }
}

fn rpc_response(message: &Value, id: u64) -> Result<Option<RpcReply>> {
    ensure!(
        message.get("jsonrpc").and_then(Value::as_str) == Some("2.0"),
        "invalid MCP JSON-RPC version"
    );
    if message.get("method").is_some() {
        return Ok(None);
    }
    ensure!(
        message.get("id").and_then(Value::as_u64) == Some(id),
        "MCP response ID mismatch"
    );
    let reply = match (message.get("result"), message.get("error")) {
        (Some(result), None) => RpcReply::Success(result.clone()),
        (None, Some(error)) => {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .context("MCP response has an invalid error code")?;
            ensure!(
                error.get("message").and_then(Value::as_str).is_some(),
                "MCP response has an invalid error message"
            );
            RpcReply::ServerError(code)
        }
        _ => bail!("MCP response must contain exactly one result or error"),
    };
    Ok(Some(reply))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Process {
        sends: Arc<AtomicUsize>,
        replies: VecDeque<Value>,
        output: Option<Vec<u8>>,
    }
    #[async_trait]
    impl RuntimeProcess for Process {
        fn id(&self) -> u32 {
            42
        }
        async fn send(&mut self, data: &[u8]) -> Result<()> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_slice(data)?;
            if let Some(result) = self.replies.pop_front() {
                self.output = Some(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    )
                    .into_bytes(),
                );
            }
            Ok(())
        }
        async fn next(&mut self) -> Result<Option<ProcessEvent>> {
            match self.output.take() {
                Some(bytes) => Ok(Some(ProcessEvent::Stdout(bytes))),
                None => std::future::pending().await,
            }
        }
        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }
    fn client(replies: Vec<Value>) -> (McpClient, Arc<AtomicUsize>) {
        let sends = Arc::new(AtomicUsize::new(0));
        (
            McpClient {
                transport: McpTransport::Stdio {
                    process: Box::new(Process {
                        sends: sends.clone(),
                        replies: replies.into(),
                        output: None,
                    }),
                    buffer: vec![],
                },
                next_id: 1,
                protocol: "2025-03-26".into(),
                closed: false,
                cleanup_complete: false,
            },
            sends,
        )
    }
    #[tokio::test]
    async fn dropping_a_request_poisoned_the_session_before_stale_replies_can_be_reused() {
        let (mut client, sends) = client(vec![]);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(5),
                client.request("tools/call", json!({}), CancellationToken::new())
            )
            .await
            .is_err()
        );
        assert!(
            client
                .request("tools/call", json!({}), CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(
            sends.load(Ordering::SeqCst),
            1,
            "no second request may reach a desynchronized transport"
        );
        client.close().await.unwrap();
    }
    #[tokio::test]
    async fn malformed_pagination_cannot_silently_truncate_the_catalog() {
        for cursor in [Value::Null, json!(false), json!(7), json!({})] {
            let (mut client, sends) = client(vec![json!({"tools":[],"nextCursor":cursor})]);
            assert!(
                client
                    .tools()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("nextCursor")
            );
            assert_eq!(sends.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn tools_catalog_has_an_aggregate_budget_across_individually_valid_pages() {
        let description = "x".repeat(5 * 1024 * 1024);
        let (mut client, sends) = client(vec![
            json!({"tools":[{"name":"one","description":description,"inputSchema":{"type":"object"}}],"nextCursor":"next"}),
            json!({"tools":[{"name":"two","description":description,"inputSchema":{"type":"object"}}]}),
        ]);
        let error = client.tools().await.unwrap_err();
        assert!(error.to_string().contains("catalog exceeds"));
        assert_eq!(sends.load(Ordering::SeqCst), 2);
    }
}
