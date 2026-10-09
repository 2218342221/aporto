use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFile {
    pub data: String,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSpec {
    pub name: String,
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, ValueRef>,
    #[serde(default)]
    pub headers: BTreeMap<String, ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tools: Option<Vec<String>>,
}

/// The immutable builtin and PTC contracts embedded by the compiler.
pub const BUILTIN_ABI: &str = "aporto.builtins/1";
pub const PTC_ABI: &str = "aporto.ptc/1";
pub const BUILTINS: &[&str] = &[
    "exec_command",
    "read_file",
    "write_file",
    "read_skill",
    "tool_search",
];

/// Only a structured object references a deployment secret. Strings are literal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ValueRef {
    Literal(String),
    Secret { secret: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub name: String,
    pub connection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextOptions>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningOptions {
    // Providers may support additional effort names; unsupported values fail there.
    pub effort: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextOptions {
    pub verbosity: TextVerbosity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextVerbosity {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase", deny_unknown_fields)]
pub enum RuntimeSpec {
    Agentenv {
        template: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        templates: Vec<String>,
        #[serde(
            default = "default_runtime_workdir",
            skip_serializing_if = "is_default_runtime_workdir"
        )]
        workdir: String,
    },
    Docker {
        image: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
        #[serde(
            default = "default_runtime_workdir",
            skip_serializing_if = "is_default_runtime_workdir"
        )]
        workdir: String,
    },
}

fn default_runtime_workdir() -> String {
    crate::workspace::DEFAULT_WORKDIR.into()
}

fn is_default_runtime_workdir(value: &str) -> bool {
    value == crate::workspace::DEFAULT_WORKDIR
}

impl RuntimeSpec {
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Agentenv { .. } => "agentenv",
            Self::Docker { .. } => "docker",
        }
    }

    pub fn reference(&self) -> &str {
        match self {
            Self::Agentenv { template, .. } => template,
            Self::Docker { image, .. } => image,
        }
    }

    /// The default is always selectable, followed by explicit alternatives in
    /// author order. A repeated default does not add a second choice.
    pub fn references(&self) -> Vec<&str> {
        let (default, additional) = match self {
            Self::Agentenv {
                template,
                templates,
                ..
            } => (template, templates),
            Self::Docker { image, images, .. } => (image, images),
        };
        let mut references = vec![default.as_str()];
        for reference in additional {
            if !references.contains(&reference.as_str()) {
                references.push(reference);
            }
        }
        references
    }

    pub fn workdir(&self) -> &str {
        match self {
            Self::Agentenv { workdir, .. } | Self::Docker { workdir, .. } => workdir,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSpec {
    pub role: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSpec {
    pub name: String,
    pub description: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_turns: u32,
    pub turn_timeout_ms: u64,
    pub cell_timeout_ms: u64,
    pub memory_mb: usize,
    pub max_output_bytes: usize,
    pub max_tool_calls: usize,
    pub max_parallel: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: 24,
            turn_timeout_ms: 900_000,
            cell_timeout_ms: 30000,
            memory_mb: 128,
            max_output_bytes: 65536,
            max_tool_calls: 64,
            max_parallel: 8,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub model: ModelSpec,
    pub runtime: RuntimeSpec,
    pub builtin_abi: String,
    pub ptc_abi: String,
    pub prompts: Vec<PromptSpec>,
    pub skills: Vec<SkillSpec>,
    pub mcp: Vec<McpSpec>,
    pub secrets: Vec<String>,
    pub limits: Limits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub format: String,
    pub digest: String,
    pub manifest: Manifest,
    pub files: BTreeMap<String, BundleFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub parallel: bool,
}

#[async_trait]
pub trait ToolBroker: Send + Sync {
    /// Fixed thread setting, supplied by the host rather than discovered in files.
    fn working_directory(&self) -> Option<&str> {
        None
    }
    fn definitions(&self) -> Vec<ToolDefinition>;
    async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> Result<Value>;
}

#[derive(Clone, Debug, Default)]
pub struct ExecOptions {
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub cancel: CancellationToken,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Clone, Debug)]
pub enum ProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

#[async_trait]
pub trait RuntimeProcess: Send {
    fn id(&self) -> u32;
    async fn send(&mut self, data: &[u8]) -> Result<()>;
    async fn next(&mut self) -> Result<Option<ProcessEvent>>;
    async fn close(&mut self) -> Result<()>;
}

#[async_trait]
pub trait Runtime: Send + Sync {
    fn id(&self) -> &str;
    async fn exec(&self, command: &str, options: ExecOptions) -> Result<CommandResult>;
    async fn start_process(
        &self,
        argv: &[String],
        options: ExecOptions,
    ) -> Result<Box<dyn RuntimeProcess>>;
    async fn read_file(&self, path: &str) -> Result<Vec<u8>>;
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()>;
    async fn close(&self) -> Result<()>;
}

/// Persistent workspaces can be quiesced between turns and reopened by ID.
/// `close` deletes the workspace; `pause` preserves its files.
#[async_trait]
pub trait ManagedRuntime: Runtime {
    async fn pause(&self) -> Result<()>;
}
