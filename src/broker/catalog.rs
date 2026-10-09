//! The published tool contract shared by execution and discovery.
use crate::types::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BrokerCatalog {
    pub builtin_abi: String,
    pub ptc_abi: String,
    pub tools: BTreeMap<String, ToolContract>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolContract {
    pub definition: ToolDefinition,
    pub origin: ToolOrigin,
    pub output_schema: Option<Value>,
    pub annotations: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolOrigin {
    Builtin,
    Mcp {
        server: String,
        name: String,
        protocol: String,
    },
}

impl BrokerCatalog {
    /// Verify a stored release catalog against its verified bundle before execution.
    /// The catalog may select all advertised tools, or the Agentfile's exact filter.
    pub fn validate(&self, bundle: &crate::types::Bundle) -> anyhow::Result<()> {
        use crate::types::{BUILTIN_ABI, PTC_ABI};
        use anyhow::{Context, ensure};
        use std::collections::BTreeSet;

        ensure!(
            self.builtin_abi == BUILTIN_ABI
                && self.ptc_abi == PTC_ABI
                && self.builtin_abi == bundle.manifest.builtin_abi
                && self.ptc_abi == bundle.manifest.ptc_abi,
            "invalid catalog ABI"
        );
        let builtins: BTreeMap<_, _> = super::builtin_definitions()
            .into_iter()
            .map(|definition| (definition.name.clone(), definition))
            .collect();
        for (name, definition) in &builtins {
            let contract = self.tools.get(name).context("catalog missing builtin")?;
            ensure!(
                contract.definition == *definition
                    && contract.origin == ToolOrigin::Builtin
                    && contract.output_schema.is_none()
                    && contract.annotations.is_none(),
                "catalog builtin contract changed: {name}"
            );
        }
        let mut selected: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut protocols = BTreeMap::new();
        for (key, contract) in &self.tools {
            ensure!(
                key == &contract.definition.name,
                "catalog key differs from callable name"
            );
            match &contract.origin {
                ToolOrigin::Builtin => {
                    ensure!(builtins.contains_key(key), "unknown catalog builtin")
                }
                ToolOrigin::Mcp {
                    server,
                    name,
                    protocol,
                } => {
                    let spec = bundle
                        .manifest
                        .mcp
                        .iter()
                        .find(|spec| spec.name == *server)
                        .context("catalog refers to undeclared MCP server")?;
                    validate_mcp_name(name)?;
                    ensure!(
                        key == &format!(
                            "mcp__{}__{}",
                            super::js_name(server),
                            super::js_name(name)
                        ),
                        "catalog MCP callable name differs from its origin"
                    );
                    ensure!(
                        !contract.definition.parallel,
                        "MCP parallel execution is not supported by this ABI"
                    );
                    ensure!(
                        matches!(
                            protocol.as_str(),
                            "2024-11-05" | "2025-03-26" | "2025-06-18"
                        ),
                        "unsupported catalog MCP protocol"
                    );
                    if let Some(previous) = protocols.insert(server.as_str(), protocol.as_str()) {
                        ensure!(previous == protocol, "inconsistent catalog MCP protocol");
                    }
                    if let Some(filter) = &spec.include_tools {
                        ensure!(
                            filter.contains(name),
                            "catalog includes a filtered MCP tool"
                        );
                    }
                    ensure!(
                        selected.entry(server).or_default().insert(name),
                        "duplicate catalog MCP origin"
                    );
                    validate_schema(&contract.definition.input_schema, "input")?;
                    if let Some(schema) = &contract.output_schema {
                        validate_schema(schema, "output")?;
                    }
                    if let Some(annotations) = &contract.annotations {
                        validate_annotations(annotations)?;
                    }
                }
            }
        }
        for spec in &bundle.manifest.mcp {
            if let Some(filter) = &spec.include_tools {
                let names = selected.get(spec.name.as_str());
                ensure!(
                    filter
                        .iter()
                        .all(|name| names.is_some_and(|names| names.contains(name.as_str()))),
                    "catalog missing selected MCP tool"
                );
            }
        }
        crate::ptc::validate_tool_catalog(
            &self
                .tools
                .values()
                .map(|c| c.definition.clone())
                .collect::<Vec<_>>(),
        )
    }
}

pub(super) fn validate_mcp_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
        "invalid MCP tool name"
    );
    Ok(())
}

pub(super) fn validate_schema(schema: &Value, kind: &str) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    super::schema_safe(schema)?;
    ensure!(
        schema.get("type").and_then(Value::as_str) == Some("object"),
        "MCP {kind} schema must have object type"
    );
    jsonschema::validator_for(schema).with_context(|| format!("invalid MCP {kind} schema"))?;
    Ok(())
}

pub(super) fn validate_annotations(annotations: &Value) -> anyhow::Result<()> {
    use anyhow::ensure;
    ensure!(annotations.is_object(), "MCP annotations must be an object");
    if let Some(title) = annotations.get("title") {
        ensure!(title.is_string(), "MCP annotation title must be text");
    }
    for key in [
        "readOnlyHint",
        "destructiveHint",
        "idempotentHint",
        "openWorldHint",
    ] {
        if let Some(value) = annotations.get(key) {
            ensure!(value.is_boolean(), "MCP annotation {key} must be boolean");
        }
    }
    Ok(())
}
