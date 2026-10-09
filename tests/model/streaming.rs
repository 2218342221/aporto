use super::*;
use tokio::sync::{Notify, mpsc};

struct StreamingObserver {
    events: mpsc::UnboundedSender<(String, Value)>,
}

#[async_trait]
impl RunObserver for StreamingObserver {
    async fn emit(&self, kind: &str, data: Value) -> Result<()> {
        self.events.send((kind.to_owned(), data)).unwrap();
        Ok(())
    }
}

struct WireReply {
    chunks: Vec<Vec<u8>>,
    pause: Option<(usize, Arc<Notify>)>,
    content_type: &'static str,
}

fn event(value: Value) -> Vec<u8> {
    format!(
        "event: {}\r\ndata: {value}\r\n\r\n",
        value["type"].as_str().unwrap()
    )
    .into_bytes()
}

fn message(text: &str, phase: Option<&str>) -> Value {
    let mut item = json!({"id":"msg-fixture", "type":"message", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text","text":text}]});
    if let Some(phase) = phase {
        item["phase"] = json!(phase);
    }
    item
}

fn delta(index: usize, text: &str) -> Vec<u8> {
    event(
        json!({"type":"response.output_text.delta", "output_index":index,"content_index":0,"delta":text}),
    )
}

fn completed(output: Vec<Value>) -> Vec<u8> {
    event(json!({"type":"response.completed","response":reply(json!(output))}))
}

fn sse(chunks: Vec<Vec<u8>>) -> WireReply {
    WireReply {
        chunks,
        pause: None,
        content_type: "text/event-stream; charset=utf-8",
    }
}

async fn wire_fixture(replies: Vec<WireReply>) -> (ModelConfig, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for reply in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let end = loop {
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..end]);
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
                let mut buffer = [0u8; 4096];
                let n = stream.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let request: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
            assert_eq!(request["stream"], true);
            let total = reply.chunks.iter().map(Vec::len).sum::<usize>();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n", reply.content_type).as_bytes()).await.unwrap();
            for (index, chunk) in reply.chunks.iter().enumerate() {
                if stream.write_all(chunk).await.is_err() {
                    break;
                }
                if let Some((pause_index, gate)) = &reply.pause
                    && *pause_index == index
                {
                    gate.notified().await;
                }
                tokio::task::yield_now().await;
            }
        }
    });
    (
        ModelConfig {
            redacted_values: vec![],
            endpoint,
            api_key: "fixture-key".into(),
            request_timeout: Duration::from_secs(5),
            headers: Default::default(),
        },
        server,
    )
}

#[tokio::test]
async fn assistant_text_is_observable_before_response_completion() {
    let gate = Arc::new(Notify::new());
    let first = [b": keepalive\r\n\r\n".as_slice(), &delta(0, "准备🦀")].concat();
    let wire = WireReply {
        chunks: vec![
            first,
            delta(0, "完成"),
            completed(vec![message("准备🦀完成", None)]),
        ],
        pause: Some((0, gate.clone())),
        content_type: "text/event-stream",
    };
    let (config, server) = wire_fixture(vec![wire]).await;
    let (events, mut observed) = mpsc::unbounded_channel();
    let observer = Arc::new(StreamingObserver { events });
    let run = tokio::spawn(async move {
        run_agent_turn(
            &fixture_bundle(),
            Arc::new(Echo {
                calls: AtomicUsize::new(0),
            }),
            config,
            "task",
            vec![],
            observer,
            CancellationToken::new(),
        )
        .await
    });
    let first_item = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (kind, value) = observed.recv().await.unwrap();
            if kind == "item.started" {
                break value["item"].clone();
            }
        }
    })
    .await
    .expect("public text must arrive while HTTP body remains open");
    assert!(!run.is_finished());
    assert_eq!(first_item["text"], "准备🦀");
    assert_eq!(first_item["phase"], "commentary");
    assert_eq!(first_item["id"], "message:1:0");
    gate.notify_one();
    let outcome = run.await.unwrap().unwrap();
    assert_eq!(outcome.run.answer, "准备🦀完成");
    let mut completions = Vec::new();
    while let Some((kind, value)) = observed.recv().await {
        if kind == "item.completed" {
            completions.push(value["item"].clone());
        }
    }
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0]["id"], first_item["id"]);
    assert_eq!(completions[0]["phase"], "final_answer");
    assert_eq!(completions[0]["text"], "准备🦀完成");
    server.await.unwrap();
}

#[tokio::test]
async fn fragmented_unicode_multiline_sse_preserves_commentary_and_final_answer() {
    let broker = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let observer = Arc::new(Observer::default());
    let first_message = message("正在核对。", Some("commentary"));
    let tool = exec_call("stream-exec", "text(await tools.echo({value:42}));");
    let first = [
        event(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}})),
        delta(0, "正在核对。"),
        event(json!({"type":"response.reasoning_summary_text.delta","output_index":1,"delta":"PRIVATE_REASONING"})),
        event(json!({"type":"response.output_item.done","output_index":0,"item":first_message})),
        event(json!({"type":"response.output_item.done","output_index":1,"item":tool})),
        event(json!({"type":"response.completed","response":{"id":"terminal-metadata"}})),
    ].concat();
    let multiline = b"event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":1,\r\ndata: \"content_index\":0,\"delta\":\"";
    let second = [
        multiline.as_slice(),
        "完成🦀".as_bytes(),
        b"\"}\r\n\r\n",
        &completed(vec![
            message("已验证 42。", Some("commentary")),
            message("完成🦀", Some("final_answer")),
        ]),
    ]
    .concat();
    let (config, server) = wire_fixture(vec![
        sse(first.chunks(3).map(<[u8]>::to_vec).collect()),
        sse(second.chunks(2).map(<[u8]>::to_vec).collect()),
    ])
    .await;
    let outcome = run_agent_turn(
        &fixture_bundle(),
        broker.clone(),
        config,
        "task",
        vec![],
        observer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(broker.calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.run.answer, "完成🦀");
    server.await.unwrap();
    let events = observer.0.lock().unwrap();
    let messages = events
        .iter()
        .filter(|(kind, data)| {
            kind == "item.completed" && data["item"]["kind"] == "assistant_message"
        })
        .map(|(_, data)| data["item"].clone())
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["phase"], "commentary");
    assert_eq!(messages[1]["text"], "已验证 42。");
    assert_eq!(messages[2]["phase"], "final_answer");
    assert!(
        !serde_json::to_string(&*events)
            .unwrap()
            .contains("PRIVATE_REASONING")
    );
}

#[tokio::test]
async fn incomplete_failed_and_truncated_streams_never_execute_tools() {
    for terminal in [
        None,
        Some("response.incomplete"),
        Some("response.failed"),
        Some("error"),
    ] {
        let broker = Arc::new(Echo {
            calls: AtomicUsize::new(0),
        });
        let mut chunks = vec![event(
            json!({"type":"response.output_item.done","output_index":0,
            "item":exec_call("blocked", "await tools.echo({});")}),
        )];
        if let Some(terminal) = terminal {
            chunks.push(event(json!({"type":terminal,"response":{"status":"incomplete"},"message":"PROVIDER_PRIVATE_BODY"})));
        }
        let (config, server) = wire_fixture(vec![sse(chunks)]).await;
        let error = run_agent(
            &fixture_bundle(),
            broker.clone(),
            config,
            "task",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(broker.calls.load(Ordering::SeqCst), 0);
        assert!(!format!("{error:#}").contains("PROVIDER_PRIVATE_BODY"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn canonical_completion_replaces_divergent_text_and_supports_refusals() {
    let observer = Arc::new(Observer::default());
    let refusal = json!({"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"无法执行。"}]});
    let (config, server) = wire_fixture(vec![sse(vec![
        event(json!({"type":"response.refusal.delta","output_index":0,"content_index":0,"delta":"临时文本"})),
        completed(vec![refusal]),
    ])]).await;
    let result = run_agent_turn(
        &fixture_bundle(),
        Arc::new(Echo {
            calls: AtomicUsize::new(0),
        }),
        config,
        "task",
        vec![],
        observer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.run.answer, "无法执行。");
    server.await.unwrap();
    let events = observer.0.lock().unwrap();
    let started = events
        .iter()
        .filter(|(kind, _)| kind == "item.started")
        .collect::<Vec<_>>();
    let completed = events
        .iter()
        .filter(|(kind, _)| kind == "item.completed")
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 1);
    assert_eq!(completed.len(), 1);
    assert_eq!(started[0].1["item"]["id"], completed[0].1["item"]["id"]);
    assert_eq!(completed[0].1["item"]["text"], "无法执行。");
}

#[tokio::test]
async fn long_stream_text_and_event_count_are_bounded_but_answer_is_complete() {
    let observer = Arc::new(Observer::default());
    let text = "🦀".repeat(20_000);
    let mut chunks = text
        .as_bytes()
        .chunks(1024)
        .map(|part| delta(0, std::str::from_utf8(part).unwrap()))
        .collect::<Vec<_>>();
    chunks.push(completed(vec![message(&text, None)]));
    let (config, server) = wire_fixture(vec![sse(chunks)]).await;
    let outcome = run_agent_turn(
        &fixture_bundle(),
        Arc::new(Echo {
            calls: AtomicUsize::new(0),
        }),
        config,
        "task",
        vec![],
        observer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.run.answer, text);
    server.await.unwrap();
    let events = observer.0.lock().unwrap();
    assert!(
        events
            .iter()
            .filter(|(kind, _)| kind == "item.updated")
            .count()
            <= 32
    );
    for (_, data) in events
        .iter()
        .filter(|(_, data)| data["item"]["kind"] == "assistant_message")
    {
        assert!(data["item"]["text"].as_str().unwrap().len() <= 32 * 1024);
    }
    let final_item = &events
        .iter()
        .find(|(kind, _)| kind == "item.completed")
        .unwrap()
        .1["item"];
    assert_eq!(final_item["truncated"], true);
}

#[tokio::test]
async fn split_credentials_are_redacted_before_stream_events_and_final_answer() {
    let secret = "sk-stream-private-credential";
    let header = "header-private-credential";
    let gate = Arc::new(Notify::new());
    let final_text = format!("checking {secret}; header {header}");
    let output = vec![
        message(&final_text, None),
        json!({"type":"reasoning","id":"private",
        "encrypted_content":"PRIVATE_ENCRYPTED_REASONING","summary":[{"type":"summary_text","text":"PRIVATE_REASONING_SUMMARY"}]}),
    ];
    let wire = WireReply {
        chunks: vec![
            delta(0, "checking sk-stream"),
            delta(0, "-private-credential"),
            completed(output),
        ],
        pause: Some((0, gate.clone())),
        content_type: "text/event-stream",
    };
    let (mut config, server) = wire_fixture(vec![wire]).await;
    config.api_key = secret.into();
    config
        .headers
        .insert("x-private-provider", header.parse().unwrap());
    let (events, mut observed) = mpsc::unbounded_channel();
    let run = tokio::spawn(async move {
        run_agent_turn(
            &fixture_bundle(),
            Arc::new(Echo {
                calls: AtomicUsize::new(0),
            }),
            config,
            "task",
            vec![],
            Arc::new(StreamingObserver { events }),
            CancellationToken::new(),
        )
        .await
    });
    let first = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (kind, data) = observed.recv().await.unwrap();
            if kind == "item.started" {
                break data;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(first["item"]["text"], "checking [redacted]");
    gate.notify_one();
    let result = run.await.unwrap().unwrap();
    assert_eq!(result.run.answer, "checking [redacted]; header [redacted]");
    // Private Responses history is retained for provider continuation, separately
    // from the durable public activity stream.
    assert!(
        serde_json::to_string(&result.history)
            .unwrap()
            .contains(secret)
    );
    let mut public = vec![first];
    while let Some((_, data)) = observed.recv().await {
        public.push(data);
    }
    let public = serde_json::to_string(&public).unwrap();
    for private in [
        secret,
        "sk-stream",
        header,
        "PRIVATE_ENCRYPTED_REASONING",
        "PRIVATE_REASONING_SUMMARY",
    ] {
        assert!(
            !public.contains(private),
            "private content reached the observer"
        );
    }
    server.await.unwrap();
}

#[tokio::test]
async fn unified_credentials_are_masked_before_partial_model_key_suffixes() {
    let gate = Arc::new(Notify::new());
    let text = "runtime-s mcp-s";
    let wire = WireReply {
        chunks: vec![delta(0, text), completed(vec![message(text, None)])],
        pause: Some((0, gate.clone())),
        content_type: "text/event-stream",
    };
    let (mut config, server) = wire_fixture(vec![wire]).await;
    config.api_key = "sk-model".into();
    config.redacted_values = vec!["runtime-s".into(), "mcp-s".into()];
    let (events, mut observed) = mpsc::unbounded_channel();
    let run = tokio::spawn(async move {
        run_agent_turn(
            &fixture_bundle(),
            Arc::new(Echo {
                calls: AtomicUsize::new(0),
            }),
            config,
            "task",
            vec![],
            Arc::new(StreamingObserver { events }),
            CancellationToken::new(),
        )
        .await
    });
    let first = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (kind, data) = observed.recv().await.unwrap();
            if kind == "item.started" {
                break data;
            }
        }
    })
    .await
    .unwrap();
    // Release the fixture even if an assertion fails, to avoid a blocked socket.
    gate.notify_one();
    assert_eq!(first["item"]["status"], "in_progress");
    assert_eq!(first["item"]["text"], "[redacted] [redacted]");
    let result = run.await.unwrap().unwrap();
    assert_eq!(result.run.answer, "[redacted] [redacted]");
    let mut completed = 0;
    while let Some((kind, data)) = observed.recv().await {
        if data["item"]["kind"] == "assistant_message" {
            assert_eq!(data["item"]["text"], "[redacted] [redacted]");
            if kind == "item.completed" {
                completed += 1;
            }
        }
    }
    assert_eq!(completed, 1);
    server.await.unwrap();
}

#[tokio::test]
async fn turn_wide_stream_update_budget_keeps_all_message_completions() {
    let observer = Arc::new(Observer::default());
    let mut chunks = Vec::new();
    let mut output = Vec::new();
    let delta_text = "a".repeat(1024);
    for index in 0..12 {
        for _ in 0..32 {
            chunks.push(delta(index, &delta_text));
        }
        output.push(message(
            &delta_text.repeat(32),
            Some(if index == 11 {
                "final_answer"
            } else {
                "commentary"
            }),
        ));
    }
    chunks.push(completed(output));
    let (config, server) = wire_fixture(vec![sse(chunks)]).await;
    run_agent_turn(
        &fixture_bundle(),
        Arc::new(Echo {
            calls: AtomicUsize::new(0),
        }),
        config,
        "task",
        vec![],
        observer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    server.await.unwrap();
    let events = observer.0.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "item.updated")
            .count(),
        300
    );
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "item.completed")
            .count(),
        12
    );
}

#[tokio::test]
async fn malformed_stream_item_lists_do_not_dispatch_tools() {
    let first = exec_call("first", "await tools.echo({});");
    let second = exec_call("second", "await tools.echo({});");
    let scenarios = [
        vec![event(json!({"type":"response.output_item.done","output_index":1,"item":second})), completed(vec![first.clone()])],
        vec![event(json!({"type":"response.output_item.done","output_index":0,"item":first.clone()})),
            event(json!({"type":"response.completed","response":{"status":"incomplete","output":[first.clone()]}}))],
        vec![b"event: response.completed\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"bad\"}\n\n".to_vec()],
        vec![completed(vec![json!({"type":"message","role":"system","content":[{"type":"output_text","text":"not assistant"}]}), first])],
    ];
    for chunks in scenarios {
        let broker = Arc::new(Echo {
            calls: AtomicUsize::new(0),
        });
        let (config, server) = wire_fixture(vec![sse(chunks)]).await;
        assert!(
            run_agent(
                &fixture_bundle(),
                broker.clone(),
                config,
                "task",
                CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(broker.calls.load(Ordering::SeqCst), 0);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn commentary_only_response_continues_to_final_answer_with_json_fallback() {
    let commentary = message("正在整理结果。", Some("commentary"));
    let observer = Arc::new(Observer::default());
    let (config, requests, server) = fixture(vec![
        reply(json!([commentary.clone()])),
        reply(json!([message("任务完成。", Some("final_answer"))])),
    ])
    .await;
    let outcome = run_agent_turn(
        &fixture_bundle(),
        Arc::new(Echo {
            calls: AtomicUsize::new(0),
        }),
        config,
        "task",
        vec![],
        observer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(outcome.run.turns, 2);
    assert_eq!(outcome.run.answer, "任务完成。");
    assert_eq!(
        requests.lock().unwrap()[1]["input"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        &commentary
    );
    let events = observer.0.lock().unwrap();
    let messages = events
        .iter()
        .filter(|(kind, data)| {
            kind == "item.completed" && data["item"]["kind"] == "assistant_message"
        })
        .map(|(_, data)| data["item"].clone())
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["text"], "正在整理结果。");
    assert_eq!(messages[0]["phase"], "commentary");
    assert_eq!(messages[1]["phase"], "final_answer");
}

#[tokio::test]
async fn empty_responses_fail_and_commentary_cannot_exceed_model_round_budget() {
    for output in [
        json!([]),
        json!([message("", Some("commentary"))]),
        json!([{"type":"reasoning","id":"private","encrypted_content":"opaque"}]),
    ] {
        let (config, requests, server) = fixture(vec![reply(output)]).await;
        assert!(
            run_agent(
                &fixture_bundle(),
                Arc::new(Echo {
                    calls: AtomicUsize::new(0)
                }),
                config,
                "task",
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("neither tool call nor final answer")
        );
        server.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 1);
    }
    let (config, requests, server) = fixture(vec![reply(json!([message(
        "仍在整理。",
        Some("commentary")
    )]))])
    .await;
    let bundle = fixture_bundle_with("", "[limits.turn]\nmax_model_requests=1");
    assert!(
        run_agent(
            &bundle,
            Arc::new(Echo {
                calls: AtomicUsize::new(0)
            }),
            config,
            "task",
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("max_turns reached")
    );
    server.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
}
