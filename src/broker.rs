//! Manifest-only tool registration. Workspace files never configure the agent.
use crate::types::{BUILTINS, Bundle, ExecOptions, Runtime, ToolBroker, ToolDefinition};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use base64::Engine;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

mod catalog;
pub use catalog::{BrokerCatalog, ToolContract, ToolOrigin};
mod mcp;
use mcp::McpClient;

enum ToolKind {
    Builtin(String),
    Mcp { server: usize, name: String },
}
struct Entry {
    definition: ToolDefinition,
    validator: jsonschema::Validator,
    kind: ToolKind,
    contract: ToolContract,
    output_validator: Option<jsonschema::Validator>,
}

pub struct Broker {
    bundle: Bundle,
    files: BTreeMap<String, Vec<u8>>,
    runtime: Arc<dyn Runtime>,
    workdir: String,
    entries: BTreeMap<String, Entry>,
    clients: Vec<Mutex<McpClient>>,
    parallel: Semaphore,
    mutation: Mutex<()>,
    calls: AtomicUsize,
    closed: AtomicBool,
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn relative_path(path: &str) -> Result<&str> {
    ensure!(
        !path.is_empty()
            && !path.starts_with('/')
            && !path.contains(['\\', '\0', '\n', '\r'])
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "invalid bundle-relative path"
    );
    Ok(path)
}

fn schema_safe(schema: &Value) -> Result<()> {
    match schema {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "$ref" || key == "$dynamicRef" {
                    ensure!(
                        value.as_str().is_some_and(|v| v.starts_with('#')),
                        "external schema references are disabled"
                    );
                }
                schema_safe(value)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                schema_safe(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn entry(definition: ToolDefinition, kind: ToolKind) -> Result<Entry> {
    schema_safe(&definition.input_schema)?;
    let validator =
        jsonschema::validator_for(&definition.input_schema).context("invalid tool input schema")?;
    let contract = ToolContract {
        definition: definition.clone(),
        origin: ToolOrigin::Builtin,
        output_schema: None,
        annotations: None,
    };
    Ok(Entry {
        definition,
        validator,
        kind,
        contract,
        output_validator: None,
    })
}

pub fn builtin_definitions() -> Vec<ToolDefinition> {
    BUILTINS
        .iter()
        .map(|name| builtin(name).expect("registered builtin"))
        .collect()
}

fn builtin(name: &str) -> Result<ToolDefinition> {
    let (description, properties, required, parallel) = match name {
        "exec_command" => (
            "Execute a shell command in the configured runtime workspace.",
            json!({"command":{"type":"string","minLength":1},"timeout_ms":{"type":"integer","minimum":1,"maximum":300000}}),
            vec!["command"],
            false,
        ),
        "read_file" => (
            "Read a UTF-8 file relative to /workspace.",
            json!({"path":{"type":"string","minLength":1}}),
            vec!["path"],
            true,
        ),
        "write_file" => (
            "Write UTF-8 text to a file relative to /workspace.",
            json!({"path":{"type":"string","minLength":1},"content":{"type":"string"}}),
            vec!["path", "content"],
            false,
        ),
        "read_skill" => (
            "Read an explicitly bundled skill by name, without scanning the workspace.",
            json!({"name":{"type":"string","minLength":1}}),
            vec!["name"],
            true,
        ),
        "tool_search" => (
            "Find callable tools by name or description. Returns input schemas and JavaScript call declarations from the published registry.",
            json!({"query":{"type":"string","maxLength":1024},"limit":{"type":"integer","minimum":1,"maximum":100}}),
            vec!["query"],
            true,
        ),
        _ => bail!("unknown builtin tool {name}"),
    };
    Ok(ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        parallel,
    })
}

fn js_name(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub async fn create_broker(
    bundle: Bundle,
    runtime: Arc<dyn Runtime>,
    secrets: BTreeMap<String, String>,
) -> Result<Broker> {
    create_broker_with_cancel(bundle, runtime, secrets, CancellationToken::new()).await
}

pub async fn create_broker_with_cancel(
    bundle: Bundle,
    runtime: Arc<dyn Runtime>,
    secrets: BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Broker> {
    let workdir = bundle.manifest.runtime.workdir().to_owned();
    create_broker_in_workspace(bundle, runtime, secrets, &workdir, cancel).await
}

pub async fn create_broker_in_workspace(
    bundle: Bundle,
    runtime: Arc<dyn Runtime>,
    secrets: BTreeMap<String, String>,
    workdir: &str,
    cancel: CancellationToken,
) -> Result<Broker> {
    let workdir = crate::workspace::validate_workdir(workdir)?;
    crate::build::verify_bundle(&bundle)?;
    ensure!(
        bundle.manifest.limits.max_parallel > 0 && bundle.manifest.limits.max_output_bytes >= 256,
        "invalid broker limits"
    );
    let mut files = BTreeMap::new();
    let mut total_bytes = 0usize;
    for (path, file) in &bundle.files {
        relative_path(path)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&file.data)
            .context("invalid bundle file encoding")?;
        total_bytes += bytes.len();
        ensure!(
            total_bytes <= 256 * 1024 * 1024,
            "bundle payload exceeds 256 MiB"
        );
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == file.sha256,
            "bundle file digest mismatch: {path}"
        );
        files.insert(path.clone(), bytes);
    }
    let mut entries = BTreeMap::new();
    for &name in BUILTINS {
        ensure!(!entries.contains_key(name), "duplicate tool {name}");
        entries.insert(
            name.to_owned(),
            entry(builtin(name)?, ToolKind::Builtin(name.to_owned()))?,
        );
    }
    let max_parallel = bundle.manifest.limits.max_parallel;
    let mut broker = Broker {
        bundle,
        files,
        runtime,
        workdir,
        entries,
        clients: Vec::new(),
        parallel: Semaphore::new(max_parallel),
        mutation: Mutex::new(()),
        calls: AtomicUsize::new(0),
        closed: AtomicBool::new(false),
    };
    // Reinstalling a bundle must work when an earlier turn made its files read-only.
    // Keep staged paths outside the future so cancellation can also clean them up.
    let mut temporary_files = Vec::new();
    let setup = async {
        let result = broker
            .runtime
            .exec(
                &format!("mkdir -p -- /opt/agent {}", quote(&broker.workdir)),
                ExecOptions {
                    cwd: Some("/".into()),
                    ..Default::default()
                },
            )
            .await?;
        ensure!(
            result.exit_code == 0,
            "runtime must allow installing packaged resources and creating the working directory"
        );
        for (path, bytes) in &broker.files {
            let destination = format!("/opt/agent/{path}");
            let parent = destination.rsplit_once('/').expect("absolute path").0;
            let result = broker
                .runtime
                .exec(
                    &format!("mkdir -p -- {}", quote(parent)),
                    ExecOptions::default(),
                )
                .await?;
            ensure!(result.exit_code == 0, "cannot create bundle directory");
            let temporary = format!("{parent}/.aporto-{}.tmp", uuid::Uuid::new_v4());
            temporary_files.push(temporary.clone());
            broker.runtime.write_file(&temporary, bytes).await?;
            let mode = if broker.bundle.files[path].mode & 0o111 != 0 {
                "0555"
            } else {
                "0444"
            };
            let result = broker
                .runtime
                .exec(
                    &format!("chmod {mode} -- {}", quote(&temporary)),
                    ExecOptions::default(),
                )
                .await?;
            ensure!(
                result.exit_code == 0,
                "cannot protect bundled file permissions"
            );
            let result = broker
                .runtime
                .exec(
                    &format!("mv -fT -- {} {}", quote(&temporary), quote(&destination)),
                    ExecOptions::default(),
                )
                .await?;
            ensure!(result.exit_code == 0, "cannot install bundled file");
            temporary_files.pop();
        }
        let mut names = BTreeSet::new();
        for spec in &broker.bundle.manifest.mcp {
            ensure!(
                names.insert(spec.name.clone()),
                "duplicate MCP server {}",
                spec.name
            );
            let client = McpClient::connect(
                spec,
                broker.runtime.clone(),
                &broker.bundle.manifest.secrets,
                &secrets,
            )
            .await?;
            let index = broker.clients.len();
            // Register before initialization so failed handshakes are cleaned up.
            broker.clients.push(Mutex::new(client));
            let mut client = broker.clients[index].lock().await;
            client.initialize().await?;
            let available = client.tools().await?;
            // Validate the advertised catalog before selecting tools. Descriptions and
            // schemas are untrusted metadata; they cannot expand framework authority.
            let mut candidates = BTreeMap::new();
            for (original, tool) in &available {
                catalog::validate_mcp_name(original)?;
                let name = format!("mcp__{}__{}", js_name(&spec.name), js_name(original));
                ensure!(
                    !candidates.contains_key(&name) && !broker.entries.contains_key(&name),
                    "tool name collision: {name}"
                );
                let description = match tool.get("description") {
                    Some(value) => value.as_str().context("MCP description must be text")?,
                    None => "MCP tool",
                };
                let definition = ToolDefinition {
                    name: name.clone(),
                    description: description.into(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .context("MCP tool lacks inputSchema")?,
                    parallel: false,
                };
                catalog::validate_schema(&definition.input_schema, "input")?;
                let mut candidate = entry(
                    definition,
                    ToolKind::Mcp {
                        server: index,
                        name: original.clone(),
                    },
                )?;
                candidate.contract.origin = ToolOrigin::Mcp {
                    server: spec.name.clone(),
                    name: original.clone(),
                    protocol: client.protocol().into(),
                };
                if let Some(schema) = tool.get("outputSchema") {
                    schema_safe(schema)?;
                    ensure!(
                        schema.get("type").and_then(Value::as_str) == Some("object"),
                        "MCP output schema must have object type"
                    );
                    candidate.output_validator = Some(
                        jsonschema::validator_for(schema).context("invalid MCP output schema")?,
                    );
                    candidate.contract.output_schema = Some(schema.clone());
                }
                if let Some(annotations) = tool.get("annotations") {
                    catalog::validate_annotations(annotations)?;
                    candidate.contract.annotations = Some(annotations.clone());
                }
                candidates.insert(name, (original, candidate));
            }
            if let Some(selected) = &spec.include_tools {
                for name in selected {
                    ensure!(
                        available.contains_key(name),
                        "MCP {} did not advertise selected tool {name}",
                        spec.name
                    );
                }
            }
            for (name, (original, candidate)) in candidates {
                if spec
                    .include_tools
                    .as_ref()
                    .is_none_or(|names| names.contains(original))
                {
                    broker.entries.insert(name, candidate);
                }
            }
        }
        broker.catalog().validate(&broker.bundle)?;
        Ok::<(), anyhow::Error>(())
    };
    let setup = tokio::select! {
        result = setup => result,
        () = cancel.cancelled() => Err(anyhow::anyhow!("broker setup cancelled")),
    };
    if let Err(error) = setup {
        for path in temporary_files {
            let _ = broker
                .runtime
                .exec(
                    &format!("rm -f -- {}", quote(&path)),
                    ExecOptions::default(),
                )
                .await;
        }
        let _ = broker.close().await;
        return Err(error);
    }
    Ok(broker)
}

impl Broker {
    pub fn catalog(&self) -> BrokerCatalog {
        BrokerCatalog {
            builtin_abi: self.bundle.manifest.builtin_abi.clone(),
            ptc_abi: self.bundle.manifest.ptc_abi.clone(),
            tools: self
                .entries
                .iter()
                .map(|(name, entry)| (name.clone(), entry.contract.clone()))
                .collect(),
        }
    }

    /// Validate the published contract before exposing any tool to a task. Tools
    /// advertised after activation are removed rather than silently authorized.
    pub fn enforce_catalog(&mut self, expected: &BrokerCatalog) -> Result<()> {
        expected
            .validate(&self.bundle)
            .context("tool_contract_changed: invalid published catalog")?;
        let current = self.catalog();
        ensure!(
            expected.builtin_abi == current.builtin_abi && expected.ptc_abi == current.ptc_abi,
            "tool_contract_changed: unsupported tool ABI"
        );
        for (name, contract) in &expected.tools {
            ensure!(
                current.tools.get(name) == Some(contract),
                "tool_contract_changed: {name}"
            );
        }
        for &name in BUILTINS {
            ensure!(
                expected.tools.contains_key(name),
                "tool_contract_changed: missing builtin {name}"
            );
        }
        self.entries
            .retain(|name, _| expected.tools.contains_key(name));
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::Release);
        let mut failure = None;
        for client in &self.clients {
            if let Err(error) = client.lock().await.close().await {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn cap(&self, value: Value) -> Result<Value> {
        let text = serde_json::to_string(&value)?;
        let maximum = self.bundle.manifest.limits.max_output_bytes;
        if text.len() <= maximum {
            return Ok(value);
        }
        // Outcome fields must survive truncation so callers and public progress
        // cannot mistake a failed command or MCP response for a successful call.
        let mut capped = json!({"truncated":true,"original_bytes":text.len(),"preview":""});
        if let Some(code) = value.get("exit_code").and_then(Value::as_i64) {
            capped["exit_code"] = json!(code);
        }
        if let Some(error) = value.get("isError").and_then(Value::as_bool) {
            capped["isError"] = json!(error);
        }
        if let Some(status) = value.get("status").filter(|status| status.is_string())
            && serde_json::to_string(status)?.len() <= 64
        {
            capped["status"] = status.clone();
        }
        let mut end = (maximum / 4).min(text.len());
        loop {
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            capped["preview"] = json!(&text[..end]);
            // Escaping and retained metadata share the same serialized budget.
            if serde_json::to_vec(&capped)?.len() <= maximum {
                return Ok(capped);
            }
            ensure!(end > 0, "tool output budget cannot fit truncation metadata");
            end /= 2;
        }
    }

    async fn builtin_call(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let string = |field: &str| -> Result<&str> {
            args.get(field)
                .and_then(Value::as_str)
                .with_context(|| format!("missing {field}"))
        };
        match name {
            "exec_command" => {
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.bundle.manifest.limits.cell_timeout_ms);
                Ok(serde_json::to_value(
                    self.runtime
                        .exec(
                            string("command")?,
                            ExecOptions {
                                cwd: Some(self.workdir.clone()),
                                timeout_ms: Some(timeout_ms),
                                cancel,
                                ..Default::default()
                            },
                        )
                        .await?,
                )?)
            }
            "read_file" => {
                let path = crate::workspace::resolve_file(&self.workdir, string("path")?)?;
                let content = tokio::select! { result = self.runtime.read_file(&path) => result?, () = cancel.cancelled() => bail!("read cancelled") };
                Ok(
                    json!({"path":path,"content":String::from_utf8(content).context("file is not UTF-8")?}),
                )
            }
            "write_file" => {
                let path = crate::workspace::resolve_file(&self.workdir, string("path")?)?;
                let content = string("content")?;
                tokio::select! { result = self.runtime.write_file(&path, content.as_bytes()) => result?, () = cancel.cancelled() => bail!("write cancelled; completion is unknown") };
                Ok(json!({"path":path,"bytes":content.len()}))
            }
            "read_skill" => {
                let name = string("name")?;
                let skill = self
                    .bundle
                    .manifest
                    .skills
                    .iter()
                    .find(|skill| skill.name == name)
                    .context("skill is not declared")?;
                let bytes = self
                    .files
                    .get(&skill.path)
                    .or_else(|| self.files.get(&format!("{}/SKILL.md", skill.path)))
                    .context("bundled skill content missing")?;
                Ok(
                    json!({"name":name,"content":std::str::from_utf8(bytes).context("skill is not UTF-8")?}),
                )
            }
            "tool_search" => {
                let query = string("query")?.to_lowercase();
                let terms: Vec<_> = query.split_whitespace().collect();
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                let tools: Vec<_> = self.entries.values().filter(|entry| {
                    let haystack = format!("{} {}", entry.definition.name, entry.definition.description).to_lowercase();
                    terms.iter().all(|term| haystack.contains(term))
                }).take(limit).map(|entry| {
                    let definition = &entry.definition;
                    json!({"name":definition.name,"description":definition.description,
                        "input_schema":definition.input_schema,
                        "declaration":format!("tools.{}(args: {}) -> Promise<JSON>",definition.name,definition.input_schema)})
                }).collect();
                Ok(json!({"tools":tools}))
            }
            _ => bail!("unknown builtin"),
        }
    }
}

#[async_trait]
impl ToolBroker for Broker {
    fn working_directory(&self) -> Option<&str> {
        Some(&self.workdir)
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.entries
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> Result<Value> {
        ensure!(!self.closed.load(Ordering::Acquire), "broker is closed");
        let entry = self
            .entries
            .get(name)
            .context("tool is not authorized by the Agentfile")?;
        ensure!(
            entry.validator.is_valid(&args),
            "tool arguments do not match the declared input schema"
        );
        ensure!(
            self.calls.fetch_add(1, Ordering::AcqRel) < self.bundle.manifest.limits.max_tool_calls,
            "tool call budget exhausted"
        );
        let _permit = tokio::select! { permit = self.parallel.acquire() => permit?, () = cancel.cancelled() => bail!("tool cancelled before dispatch") };
        let _mutation = if entry.definition.parallel {
            None
        } else {
            Some(
                tokio::select! { guard = self.mutation.lock() => guard, () = cancel.cancelled() => bail!("tool cancelled before dispatch") },
            )
        };
        ensure!(!cancel.is_cancelled(), "tool cancelled before dispatch");
        ensure!(!self.closed.load(Ordering::Acquire), "broker is closed");
        let value = match &entry.kind {
            ToolKind::Builtin(name) => self.builtin_call(name, args, cancel).await?,
            ToolKind::Mcp { server, name } => {
                let mut client = self.clients[*server].lock().await;
                let result = client
                    .request("tools/call", json!({"name":name,"arguments":args}), cancel)
                    .await;
                if result.is_err() && !client.is_usable() {
                    let _ = client.close().await;
                }
                result?
            }
        };
        if let Some(validator) = &entry.output_validator
            && value.get("isError").and_then(Value::as_bool) != Some(true)
        {
            ensure!(
                value
                    .get("structuredContent")
                    .is_some_and(|content| validator.is_valid(content)),
                "MCP result does not match the published output schema"
            );
        }
        self.cap(value)
    }
}
