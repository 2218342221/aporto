//! Broker contracts use an explicit fake guest; no host MCP subprocess is launched.
use anyhow::{Result, bail};
use aporto::{broker::create_broker, types::*};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Guest {
    reads: AtomicUsize,
    read_paths: Mutex<Vec<String>>,
    commands: Mutex<Vec<(String, Option<String>)>>,
    command_results: BTreeMap<String, CommandResult>,
    starts: Mutex<Vec<(Vec<String>, ExecOptions)>>,
    writes: Mutex<BTreeMap<String, Vec<u8>>>,
    read_only: Mutex<BTreeSet<String>>,
    fail_rename: AtomicBool,
    closes: Arc<AtomicUsize>,
    schema: Option<Value>,
    advertised: Option<Vec<Value>>,
    call_result: Option<Value>,
    stall_initialize: bool,
    initialize_started: Arc<tokio::sync::Notify>,
}
struct GuestProcess {
    pending: VecDeque<ProcessEvent>,
    closes: Arc<AtomicUsize>,
    schema: Value,
    advertised: Option<Vec<Value>>,
    call_result: Option<Value>,
    closed: bool,
    stall_initialize: bool,
    initialize_started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl RuntimeProcess for GuestProcess {
    fn id(&self) -> u32 {
        123
    }
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        let request: Value = serde_json::from_slice(data)?;
        let Some(id) = request.get("id") else {
            return Ok(());
        };
        if request["method"] == "initialize" && self.stall_initialize {
            self.initialize_started.notify_one();
            return Ok(());
        }
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
            }
            "tools/list" => {
                json!({"tools":self.advertised.clone().unwrap_or_else(|| vec![json!({"name":"echo","description":"echo","inputSchema":self.schema}),json!({"name":"denied","inputSchema":{"type":"object"}})])})
            }
            "tools/call" => {
                self.call_result.clone().unwrap_or_else(|| json!({"content":[{"type":"text","text":request["params"]["arguments"]["text"]}]}))
            }
            _ => bail!("unexpected MCP method"),
        };
        let mut encoded = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result}))?;
        encoded.push(b'\n');
        // Arbitrary stdout fragments must be joined before parsing JSON-RPC.
        for part in encoded.chunks(if encoded.len() > 100_000 { 4096 } else { 3 }) {
            self.pending.push_back(ProcessEvent::Stdout(part.to_vec()));
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<ProcessEvent>> {
        if self.pending.is_empty() && self.stall_initialize {
            std::future::pending::<()>().await;
        }
        Ok(self.pending.pop_front())
    }
    async fn close(&mut self) -> Result<()> {
        if !self.closed {
            self.closes.fetch_add(1, Ordering::SeqCst);
            self.closed = true;
        }
        Ok(())
    }
}

#[async_trait]
impl Runtime for Guest {
    fn id(&self) -> &str {
        "explicit-fake-guest"
    }
    async fn exec(&self, command: &str, options: ExecOptions) -> Result<CommandResult> {
        self.commands
            .lock()
            .unwrap()
            .push((command.into(), options.cwd));
        if let Some(result) = self.command_results.get(command) {
            return Ok(result.clone());
        }
        // This fixture has only simple paths. Model non-root file permissions while
        // allowing replacement through the guest-owned, writable parent directory.
        let arguments = command
            .split_whitespace()
            .map(|argument| argument.trim_matches('\''))
            .collect::<Vec<_>>();
        let mut writes = self.writes.lock().unwrap();
        let mut read_only = self.read_only.lock().unwrap();
        match arguments.as_slice() {
            ["chmod", "0444" | "0555", "--", path] => {
                assert!(writes.contains_key(*path));
                read_only.insert((*path).into());
            }
            ["mv", "-fT", "--", source, destination] => {
                if self.fail_rename.load(Ordering::SeqCst) {
                    return Ok(CommandResult {
                        stdout: String::new(),
                        stderr: "fixture rename failure".into(),
                        exit_code: 1,
                    });
                }
                let contents = writes.remove(*source).expect("staged file exists");
                writes.insert((*destination).into(), contents);
                read_only.remove(*destination);
                if read_only.remove(*source) {
                    read_only.insert((*destination).into());
                }
            }
            ["rm", "-f", "--", path] => {
                writes.remove(*path);
                read_only.remove(*path);
            }
            _ => {}
        }
        Ok(CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        })
    }
    async fn start_process(
        &self,
        argv: &[String],
        options: ExecOptions,
    ) -> Result<Box<dyn RuntimeProcess>> {
        self.starts.lock().unwrap().push((argv.to_vec(), options));
        Ok(Box::new(GuestProcess { pending:VecDeque::new(),closes:self.closes.clone(),advertised:self.advertised.clone(),call_result:self.call_result.clone(),schema:self.schema.clone().unwrap_or(json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false})),closed:false,stall_initialize:self.stall_initialize,initialize_started:self.initialize_started.clone() }))
    }
    async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        self.read_paths.lock().unwrap().push(path.into());
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(b"untrusted workspace instruction".to_vec())
    }
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        if self.read_only.lock().unwrap().contains(path) {
            bail!("permission denied: fixture file is read-only");
        }
        self.writes.lock().unwrap().insert(path.into(), data.into());
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

fn bundle() -> Bundle {
    bundle_with_filter(Some(&["echo"]))
}

#[tokio::test]
async fn selected_workdir_controls_commands_and_files_but_not_packaged_mcp() {
    let guest = Arc::new(Guest::default());
    let workdir = "/projects/a user's 中文";
    let broker = aporto::broker::create_broker_in_workspace(
        bundle(),
        guest.clone(),
        BTreeMap::from([("MCP_TOKEN".into(), "server-secret".into())]),
        workdir,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(broker.working_directory(), Some(workdir));
    broker
        .call(
            "exec_command",
            json!({"command":"pwd"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let result = broker
        .call(
            "read_file",
            json!({"path":"result.txt"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["path"], format!("{workdir}/result.txt"));
    assert_eq!(
        guest.read_paths.lock().unwrap().last().unwrap(),
        &format!("{workdir}/result.txt")
    );
    broker
        .call(
            "write_file",
            json!({"path":"result.txt","content":"receipt"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        guest.writes.lock().unwrap()[&format!("{workdir}/result.txt")],
        b"receipt"
    );
    assert!(
        guest
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|(command, cwd)| command == "pwd" && cwd.as_deref() == Some(workdir))
    );
    assert!(
        guest
            .starts
            .lock()
            .unwrap()
            .iter()
            .all(|(_, options)| options.cwd.as_deref().unwrap().starts_with("/opt/agent/"))
    );
    for path in [
        "/workspace/result.txt",
        "/projects/other/result.txt",
        "../result.txt",
    ] {
        assert!(
            broker
                .call("read_file", json!({"path":path}), CancellationToken::new())
                .await
                .is_err()
        );
    }
    broker.close().await.unwrap();
}

fn bundle_with_filter(filter: Option<&[&str]>) -> Bundle {
    bundle_with_options(filter, "")
}

fn bundle_with_options(filter: Option<&[&str]>, options: &str) -> Bundle {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("review")).unwrap();
    std::fs::create_dir(root.path().join("server")).unwrap();
    std::fs::write(
        root.path().join("review/SKILL.md"),
        "---\nname: review\ndescription: review\n---\n# Trusted skill\nUse the bundled procedure.",
    )
    .unwrap();
    std::fs::write(
        root.path().join("server/server.py"),
        "# Explicit fake server input; not executed by this test",
    )
    .unwrap();
    let mut source = r#"
[agent]
name = "fixture"
[model]
name = "model"
connection = "primary"
[runtime]
provider = "agentenv"
template = "test"
[[skills]]
source = "review"
[[mcp]]
name = "fixture"
source = "server"
command = ["python3", "server.py"]
env = { TOKEN = { secret = "MCP_TOKEN" } }
"#
    .to_owned();
    if let Some(filter) = filter {
        source += &format!(
            "include_tools = {}\n",
            serde_json::to_string(filter).unwrap()
        );
    }
    source += options;
    std::fs::write(root.path().join("Agentfile"), source).unwrap();
    aporto::build::build(
        root.path(),
        std::path::Path::new("Agentfile"),
        &BTreeMap::new(),
    )
    .unwrap()
}

#[tokio::test]
async fn truncated_command_and_mcp_results_preserve_failure_status() {
    for maximum in [256, Limits::default().max_output_bytes] {
        let bundle = bundle_with_options(
            Some(&["echo"]),
            &format!("\n[limits.ptc]\nmax_output_bytes = {maximum}\n"),
        );
        let large_output = "\"\\\n中".repeat(maximum);
        let broker = create_broker(
            bundle,
            Arc::new(Guest {
                command_results: BTreeMap::from([(
                    "failing-command".into(),
                    CommandResult {
                        stdout: large_output.clone(),
                        stderr: "command failed".into(),
                        exit_code: 17,
                    },
                )]),
                call_result: Some(json!({
                    "isError":true,"status":"failed",
                    "content":[{"type":"text","text":large_output}],
                })),
                ..Default::default()
            }),
            secrets(),
        )
        .await
        .unwrap();
        let command = broker
            .call(
                "exec_command",
                json!({"command":"failing-command"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mcp = broker
            .call(
                "mcp__fixture__echo",
                json!({"text":"test"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(command["exit_code"], 17);
        assert_eq!(mcp["isError"], true);
        assert_eq!(mcp["status"], "failed");
        for result in [command, mcp] {
            assert_eq!(result["truncated"], true);
            assert!(result["original_bytes"].as_u64().unwrap() > maximum as u64);
            assert!(!result["preview"].as_str().unwrap().is_empty());
            assert!(serde_json::to_vec(&result).unwrap().len() <= maximum);
        }
        broker.close().await.unwrap();
    }
}

#[tokio::test]
async fn truncated_status_metadata_counts_towards_the_output_budget() {
    let status = "\"".repeat(31);
    let broker = create_broker(
        bundle_with_options(Some(&["echo"]), "\n[limits.ptc]\nmax_output_bytes = 256\n"),
        Arc::new(Guest {
            call_result: Some(json!({
                "content":[{"type":"text","text":"\\".repeat(2048)}],
                "exit_code":i64::MIN,"isError":true,"status":status,
            })),
            ..Default::default()
        }),
        secrets(),
    )
    .await
    .unwrap();
    let result = broker
        .call(
            "mcp__fixture__echo",
            json!({"text":"test"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result["exit_code"], i64::MIN);
    assert_eq!(result["isError"], true);
    assert_eq!(result["status"], status);
    assert_eq!(result["truncated"], true);
    assert!(serde_json::to_vec(&result).unwrap().len() <= 256);
    broker.close().await.unwrap();
}

#[tokio::test]
async fn loads_only_declared_tools_and_keeps_skill_source_outside_guest() {
    let guest = Arc::new(Guest::default());
    let broker = create_broker(
        bundle(),
        guest.clone(),
        BTreeMap::from([
            ("MCP_TOKEN".into(), "server-secret".into()),
            ("OTHER_TOKEN".into(), "unrelated-secret".into()),
        ]),
    )
    .await
    .unwrap();
    let names = broker
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "exec_command",
            "mcp__fixture__echo",
            "read_file",
            "read_skill",
            "tool_search",
            "write_file"
        ]
    );
    let value = broker
        .call(
            "read_skill",
            json!({"name":"review"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(value["content"].as_str().unwrap().contains("Trusted skill"));
    assert_eq!(guest.reads.load(Ordering::SeqCst), 0);
    {
        let starts = guest.starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].0, vec!["python3", "server.py"]);
        assert_eq!(starts[0].1.cwd.as_deref(), Some("/opt/agent/mcp/fixture"));
        assert_eq!(
            starts[0].1.env,
            BTreeMap::from([("TOKEN".into(), "server-secret".into())])
        );
    }
    let value = broker
        .call(
            "mcp__fixture__echo",
            json!({"text":"hello"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(value["content"][0]["text"], "hello");
    assert!(
        broker
            .call("mcp__fixture__denied", json!({}), CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        broker
            .call(
                "mcp__fixture__echo",
                json!({"text":2}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        broker
            .call(
                "read_file",
                json!({"path":"../AGENTS.md"}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    broker.close().await.unwrap();
    assert_eq!(guest.closes.load(Ordering::SeqCst), 1);
    assert!(
        broker
            .call(
                "read_skill",
                json!({"name":"review"}),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn reinstall_replaces_read_only_files_and_retains_trusted_skill_source() {
    let guest = Arc::new(Guest::default());
    let bundle = bundle();
    let secrets = BTreeMap::from([("MCP_TOKEN".into(), "secret".into())]);
    let first = create_broker(bundle.clone(), guest.clone(), secrets.clone())
        .await
        .unwrap();
    let installed = guest.writes.lock().unwrap().clone();
    assert!(!installed.is_empty());
    assert_eq!(guest.read_only.lock().unwrap().len(), installed.len());
    for path in installed.keys() {
        assert!(guest.write_file(path, b"direct overwrite").await.is_err());
    }
    first.close().await.unwrap();

    let second = create_broker(bundle, guest.clone(), secrets).await.unwrap();
    assert_eq!(*guest.writes.lock().unwrap(), installed);
    assert_eq!(guest.read_only.lock().unwrap().len(), installed.len());
    assert!(
        guest
            .writes
            .lock()
            .unwrap()
            .keys()
            .all(|path| !path.contains(".aporto-"))
    );
    // A guest owning the directory could replace its copy after installation.
    // Prompt-bearing tools must continue to use verified host bundle bytes.
    for contents in guest.writes.lock().unwrap().values_mut() {
        *contents = b"untrusted guest replacement".to_vec();
    }
    let skill = second
        .call(
            "read_skill",
            json!({"name":"review"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(skill["content"].as_str().unwrap().contains("Trusted skill"));
    assert_eq!(guest.reads.load(Ordering::SeqCst), 0);
    second.close().await.unwrap();
    assert_eq!(guest.closes.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_reinstall_preserves_previous_files_and_removes_staged_file() {
    let guest = Arc::new(Guest::default());
    let bundle = bundle();
    let secrets = BTreeMap::from([("MCP_TOKEN".into(), "secret".into())]);
    let first = create_broker(bundle.clone(), guest.clone(), secrets.clone())
        .await
        .unwrap();
    first.close().await.unwrap();
    let installed = guest.writes.lock().unwrap().clone();
    let permissions = guest.read_only.lock().unwrap().clone();
    guest.fail_rename.store(true, Ordering::SeqCst);
    let error = create_broker(bundle, guest.clone(), secrets)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("cannot install bundled file"));
    assert_eq!(*guest.writes.lock().unwrap(), installed);
    assert_eq!(*guest.read_only.lock().unwrap(), permissions);
    assert_eq!(guest.starts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn mcp_bad_schema_closes_guest_process_without_external_schema_fetch() {
    let guest = Arc::new(Guest {
        schema: Some(json!({"type":"object","$ref":"http://127.0.0.1:1/remote-schema"})),
        ..Default::default()
    });
    let result = create_broker(
        bundle(),
        guest.clone(),
        BTreeMap::from([("MCP_TOKEN".into(), "secret".into())]),
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("external schema")
    );
    assert_eq!(guest.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn undeclared_secret_and_invalid_file_digest_fail_closed() {
    let guest = Arc::new(Guest::default());
    let mut invalid = bundle();
    invalid.manifest.secrets.clear();
    assert!(
        create_broker(
            invalid,
            guest.clone(),
            BTreeMap::from([("MCP_TOKEN".into(), "secret".into())])
        )
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("undeclared secret")
    );
    assert!(guest.starts.lock().unwrap().is_empty());
    let mut invalid = bundle();
    invalid.files.values_mut().next().unwrap().sha256 = "invalid".into();
    assert!(
        create_broker(invalid, guest, BTreeMap::new())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("checksum mismatch")
    );
}

#[tokio::test]
async fn cancelled_initialization_closes_the_started_mcp_process() {
    let guest = Arc::new(Guest {
        stall_initialize: true,
        ..Default::default()
    });
    let cancel = CancellationToken::new();
    let cancel_task = cancel.clone();
    let task_guest = guest.clone();
    let bundle = bundle();
    let setup = tokio::spawn(async move {
        aporto::broker::create_broker_with_cancel(
            bundle,
            task_guest,
            BTreeMap::from([("MCP_TOKEN".into(), "secret".into())]),
            cancel_task,
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        guest.initialize_started.notified(),
    )
    .await
    .unwrap();
    cancel.cancel();
    let error = tokio::time::timeout(std::time::Duration::from_secs(3), setup)
        .await
        .unwrap()
        .unwrap()
        .err()
        .unwrap();
    assert!(error.to_string().contains("setup cancelled"));
    assert_eq!(guest.closes.load(Ordering::SeqCst), 1);
}

fn echo_definition() -> Value {
    json!({"name":"echo","description":"echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}})
}

fn secrets() -> BTreeMap<String, String> {
    BTreeMap::from([("MCP_TOKEN".into(), "fixture-secret".into())])
}

#[tokio::test]
async fn discovery_search_and_ptc_use_the_same_callable_registry() {
    let bundle = bundle_with_filter(None);
    let broker = Arc::new(
        create_broker(bundle.clone(), Arc::new(Guest::default()), secrets())
            .await
            .unwrap(),
    );
    let definitions = broker.definitions();
    assert!(definitions.iter().any(|d| d.name == "mcp__fixture__denied"));
    let search = broker
        .call(
            "tool_search",
            json!({"query":"","limit":100}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    for definition in &definitions {
        let found = search["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == definition.name)
            .unwrap();
        assert_eq!(found["input_schema"], definition.input_schema);
        assert_eq!(found["description"], definition.description);
        assert!(
            found["declaration"]
                .as_str()
                .unwrap()
                .starts_with(&format!("tools.{}(", definition.name))
        );
    }
    assert_eq!(search["tools"].as_array().unwrap().len(), definitions.len());
    let session = aporto::ptc::PtcSession::new(broker.clone(), bundle.manifest.limits);
    let result = session
        .exec(
            r#"
        const found = await tools.tool_search({query:'mcp__fixture__echo'});
        const tool = found.tools[0];
        text(ALL_TOOLS.every(item => typeof tools[item.name] === 'function'));
        text(ALL_TOOLS.find(item=>item.name === tool.name).description === tool.description);
        text((await tools[tool.name]({text:'discovered-call'})).content[0].text);
    "#,
            aporto::ptc::ObserveOptions {
                yield_time_ms: Some(1000),
                max_output_bytes: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        result.status,
        aporto::ptc::CellStatus::Completed,
        "{:?}",
        result.error
    );
    assert_eq!(result.output, ["true", "true", "discovered-call"]);
    session.close().await.unwrap();
    broker.close().await.unwrap();
}

#[tokio::test]
async fn published_catalog_ignores_new_tools_and_rejects_contract_changes() {
    let bundle = bundle_with_filter(None);
    let mut tool = echo_definition();
    tool["annotations"] = json!({"readOnlyHint":true});
    tool["outputSchema"] = json!({"type":"object","properties":{"value":{"type":"string"}}});
    let initial = create_broker(
        bundle.clone(),
        Arc::new(Guest {
            advertised: Some(vec![tool.clone()]),
            ..Default::default()
        }),
        secrets(),
    )
    .await
    .unwrap();
    let published = initial.catalog();
    published.validate(&bundle).unwrap();
    initial.close().await.unwrap();
    let mut added = create_broker(
        bundle.clone(),
        Arc::new(Guest {
            advertised: Some(vec![
                tool.clone(),
                json!({"name":"new","inputSchema":{"type":"object"}}),
            ]),
            ..Default::default()
        }),
        secrets(),
    )
    .await
    .unwrap();
    added.enforce_catalog(&published).unwrap();
    assert!(
        added
            .definitions()
            .iter()
            .all(|d| d.name != "mcp__fixture__new")
    );
    assert!(
        added
            .call("mcp__fixture__new", json!({}), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(added.catalog(), published);
    added.close().await.unwrap();

    let mut cases = vec![vec![]];
    for (field, replacement) in [
        ("description", json!("changed")),
        (
            "inputSchema",
            json!({"type":"object","properties":{"other":{"type":"string"}}}),
        ),
        (
            "outputSchema",
            json!({"type":"object","properties":{"value":{"type":"integer"}}}),
        ),
        ("annotations", json!({"readOnlyHint":false})),
    ] {
        let mut changed = tool.clone();
        changed[field] = replacement;
        cases.push(vec![changed]);
    }
    for advertised in cases {
        let mut broker = create_broker(
            bundle.clone(),
            Arc::new(Guest {
                advertised: Some(advertised),
                ..Default::default()
            }),
            secrets(),
        )
        .await
        .unwrap();
        assert!(
            broker
                .enforce_catalog(&published)
                .unwrap_err()
                .to_string()
                .contains("tool_contract_changed")
        );
        broker.close().await.unwrap();
    }
}

#[tokio::test]
async fn unselected_metadata_and_normalized_collisions_are_validated_before_publication() {
    for invalid in [
        json!({"name":"unselected","description":5,"inputSchema":{"type":"object"}}),
        json!({"name":"unselected","inputSchema":{"type":"object","$ref":"https://invalid.example/schema"}}),
        json!({"name":"unselected","inputSchema":{"type":"object"},"outputSchema":{"type":"string"}}),
        json!({"name":"unselected","inputSchema":{"type":"object"},"annotations":[]}),
        json!({"name":"unselected","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":"true"}}),
        json!({"name":"unselected","inputSchema":{"type":"object"},"annotations":{"title":42}}),
    ] {
        let guest = Arc::new(Guest {
            advertised: Some(vec![echo_definition(), invalid]),
            ..Default::default()
        });
        assert!(
            create_broker(bundle(), guest.clone(), secrets())
                .await
                .is_err()
        );
        assert_eq!(guest.closes.load(Ordering::SeqCst), 1);
    }
    let guest = Arc::new(Guest {
        advertised: Some(vec![
            echo_definition(),
            json!({"name":"name-a","inputSchema":{"type":"object"}}),
            json!({"name":"name/a","inputSchema":{"type":"object"}}),
        ]),
        ..Default::default()
    });
    let error = create_broker(bundle(), guest.clone(), secrets())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("collision"));
    assert_eq!(guest.closes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn catalog_cannot_publish_unusable_ptc_budget_or_tampered_contracts() {
    let bundle = bundle_with_filter(None);
    let mut huge = echo_definition();
    huge["description"] = "x".repeat(2 * 1024 * 1024).into();
    let guest = Arc::new(Guest {
        advertised: Some(vec![huge]),
        ..Default::default()
    });
    assert!(
        create_broker(bundle.clone(), guest.clone(), secrets())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("tool catalog exceeds")
    );
    assert_eq!(guest.closes.load(Ordering::SeqCst), 1);
    let broker = create_broker(bundle.clone(), Arc::new(Guest::default()), secrets())
        .await
        .unwrap();
    let original = broker.catalog();
    for modification in 0..5 {
        let mut catalog = original.clone();
        match modification {
            0 => {
                catalog
                    .tools
                    .get_mut("exec_command")
                    .unwrap()
                    .definition
                    .description = "tampered".into();
            }
            1 => {
                catalog
                    .tools
                    .get_mut("mcp__fixture__echo")
                    .unwrap()
                    .definition
                    .name = "alias".into();
            }
            2 => {
                catalog
                    .tools
                    .get_mut("mcp__fixture__echo")
                    .unwrap()
                    .definition
                    .parallel = true;
            }
            3 => {
                catalog.tools.get_mut("mcp__fixture__echo").unwrap().origin =
                    aporto::broker::ToolOrigin::Mcp {
                        server: "undeclared".into(),
                        name: "echo".into(),
                        protocol: "2025-03-26".into(),
                    };
            }
            _ => {
                catalog
                    .tools
                    .get_mut("mcp__fixture__echo")
                    .unwrap()
                    .definition
                    .input_schema =
                    json!({"type":"object","$ref":"https://invalid.example/schema"});
            }
        }
        assert!(
            catalog.validate(&bundle).is_err(),
            "accepted alteration {modification}"
        );
    }
    broker.close().await.unwrap();
}

#[tokio::test]
async fn published_output_schema_is_enforced_on_success_results() {
    let mut tool = echo_definition();
    tool["outputSchema"] =
        json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"]});
    for (result, accepted) in [
        (json!({"content":[],"structuredContent":{"value":42}}), true),
        (
            json!({"content":[],"structuredContent":{"value":"invalid"}}),
            false,
        ),
        (json!({"content":[]}), false),
        (
            json!({"isError":true,"content":[{"type":"text","text":"domain error"}]}),
            true,
        ),
    ] {
        let broker = create_broker(
            bundle(),
            Arc::new(Guest {
                advertised: Some(vec![tool.clone()]),
                call_result: Some(result),
                ..Default::default()
            }),
            secrets(),
        )
        .await
        .unwrap();
        assert_eq!(
            broker
                .call(
                    "mcp__fixture__echo",
                    json!({"text":"test"}),
                    CancellationToken::new()
                )
                .await
                .is_ok(),
            accepted
        );
        broker.close().await.unwrap();
    }
}
