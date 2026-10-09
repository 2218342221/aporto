//! Author-facing, strict TOML configuration. No host or workspace discovery occurs here.
use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::Deserialize;

use crate::types::{Limits, ModelSpec, RuntimeSpec, ValueRef};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agentfile {
    pub agent: AgentInput,
    pub model: ModelSpec,
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub prompts: Vec<PromptInput>,
    #[serde(default)]
    pub skills: Vec<SkillInput>,
    #[serde(default)]
    pub mcp: Vec<McpInput>,
    #[serde(default)]
    pub assets: Vec<AssetInput>,
    #[serde(default)]
    pub limits: LimitsInput,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInput {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptInput {
    pub source: String,
    #[serde(default)]
    pub role: PromptRole,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    System,
    #[default]
    Developer,
}

impl PromptRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInput {
    pub source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetInput {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpInput {
    pub name: String,
    pub source: Option<String>,
    pub command: Option<Vec<String>>,
    pub url: Option<String>,
    pub env: Option<BTreeMap<String, ValueRef>>,
    pub headers: Option<BTreeMap<String, ValueRef>>,
    pub include_tools: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsInput {
    #[serde(default)]
    pub turn: TurnLimitsInput,
    #[serde(default)]
    pub ptc: PtcLimitsInput,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnLimitsInput {
    pub timeout_ms: Option<u64>,
    pub max_model_requests: Option<u32>,
    pub max_tool_calls: Option<usize>,
    pub max_parallel_tools: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PtcLimitsInput {
    pub cell_timeout_ms: Option<u64>,
    pub memory_mb: Option<usize>,
    pub max_output_bytes: Option<usize>,
}

impl LimitsInput {
    pub fn resolve(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_turns: self.turn.max_model_requests.unwrap_or(defaults.max_turns),
            turn_timeout_ms: self.turn.timeout_ms.unwrap_or(defaults.turn_timeout_ms),
            max_tool_calls: self.turn.max_tool_calls.unwrap_or(defaults.max_tool_calls),
            max_parallel: self
                .turn
                .max_parallel_tools
                .unwrap_or(defaults.max_parallel),
            cell_timeout_ms: self.ptc.cell_timeout_ms.unwrap_or(defaults.cell_timeout_ms),
            memory_mb: self.ptc.memory_mb.unwrap_or(defaults.memory_mb),
            max_output_bytes: self
                .ptc
                .max_output_bytes
                .unwrap_or(defaults.max_output_bytes),
        }
    }
}

/// Parse standard TOML, rejecting unknown fields, duplicate keys and invalid scalar types.
/// Source containment and final bundle integrity are validated by the offline compiler.
pub fn parse(source: &str) -> Result<Agentfile> {
    ensure!(source.len() <= 1024 * 1024, "Agentfile exceeds 1 MiB");
    let mut input: Agentfile = toml::from_str(source).map_err(|error: toml::de::Error| {
        // Parser diagnostics can contain the entire rejected line, including credentials.
        let offset = error.span().map_or(0, |span| span.start).min(source.len());
        let prefix = &source[..source.floor_char_boundary(offset)];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count()
            + 1;
        anyhow::anyhow!("invalid Agentfile TOML at line {line}, column {column}")
    })?;
    validate_model(&input.model)?;
    normalize_runtime(&mut input.runtime)?;
    validate_limits(&input.limits.resolve())?;
    for mcp in &input.mcp {
        ensure!(
            mcp.command.is_some() != mcp.url.is_some(),
            "MCP requires exactly one of command or url"
        );
        if mcp.command.is_some() {
            ensure!(mcp.headers.is_none(), "stdio MCP cannot have headers");
        } else {
            ensure!(
                mcp.source.is_none() && mcp.env.is_none(),
                "HTTP MCP cannot have source or env"
            );
        }
    }
    Ok(input)
}

fn normalize_runtime(runtime: &mut RuntimeSpec) -> Result<()> {
    let workdir = match runtime {
        RuntimeSpec::Agentenv { workdir, .. } | RuntimeSpec::Docker { workdir, .. } => workdir,
    };
    *workdir = crate::workspace::validate_workdir(workdir)?;
    validate_runtime(runtime)?;
    match runtime {
        RuntimeSpec::Agentenv {
            template,
            templates,
            ..
        } => templates.retain(|value| value != template),
        RuntimeSpec::Docker { image, images, .. } => images.retain(|value| value != image),
    }
    Ok(())
}

pub(crate) fn validate_runtime(runtime: &RuntimeSpec) -> Result<()> {
    let additional = match runtime {
        RuntimeSpec::Agentenv { templates, .. } => templates,
        RuntimeSpec::Docker { images, .. } => images,
    };
    ensure!(
        additional.len() <= 16,
        "runtime permits at most 16 unique references"
    );
    ensure!(
        additional
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == additional.len(),
        "duplicate runtime reference in allowlist"
    );
    let references = runtime.references();
    ensure!(
        references.len() <= 16,
        "runtime permits at most 16 unique references"
    );
    for reference in references {
        ensure!(
            !reference.trim().is_empty()
                && reference.len() <= 512
                && !reference.chars().any(char::is_control),
            "invalid runtime reference"
        );
        if runtime.provider() == "docker" {
            ensure!(
                !reference.starts_with('-') && !reference.chars().any(char::is_whitespace),
                "Docker image must be a reference without whitespace or a leading option prefix"
            );
        }
    }
    ensure!(
        crate::workspace::validate_workdir(runtime.workdir())? == runtime.workdir(),
        "runtime workdir must be canonical"
    );
    Ok(())
}

pub(crate) fn validate_model(model: &ModelSpec) -> Result<()> {
    ensure!(
        !model.name.trim().is_empty()
            && model.name.len() <= 512
            && !model.name.chars().any(char::is_control),
        "invalid model name"
    );
    ensure!(
        model
            .max_output_tokens
            .is_none_or(|value| (1..=1_048_576).contains(&value)),
        "max_output_tokens outside supported bounds"
    );
    if let Some(reasoning) = &model.reasoning {
        ensure!(
            !reasoning.effort.trim().is_empty()
                && reasoning.effort.len() <= 32
                && !reasoning.effort.chars().any(char::is_control),
            "invalid reasoning effort"
        );
    }
    Ok(())
}

pub(crate) fn validate_limits(limits: &Limits) -> Result<()> {
    ensure!(
        (1..=1000).contains(&limits.max_turns)
            && (1..=86_400_000).contains(&limits.turn_timeout_ms)
            && (1..=600_000).contains(&limits.cell_timeout_ms)
            && (8..=1024).contains(&limits.memory_mb)
            && (256..=16 * 1024 * 1024).contains(&limits.max_output_bytes)
            && (1..=4096).contains(&limits.max_tool_calls)
            && (1..=64).contains(&limits.max_parallel),
        "limits outside supported bounds"
    );
    Ok(())
}
