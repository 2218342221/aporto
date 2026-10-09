//! Explicit operator bindings. TOML contains environment handles, never resolved secrets.
use crate::{
    build::read_bundle,
    model::ModelConfig,
    runtime::{BackendConfig, DockerConfig, RuntimeConfig, RuntimeMode},
    types::Bundle,
};
use anyhow::{Context, Result, ensure};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvRef {
    pub env: String,
}

impl EnvRef {
    fn validate(&self) -> Result<()> {
        let mut bytes = self.env.bytes();
        ensure!(
            self.env.len() <= 128
                && bytes
                    .next()
                    .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid environment variable name"
        );
        Ok(())
    }

    pub fn resolve(&self) -> Result<String> {
        self.validate()?;
        let value = std::env::var(&self.env)
            .with_context(|| format!("missing operator environment variable {}", self.env))?;
        ensure!(
            !value.is_empty(),
            "empty operator environment variable {}",
            self.env
        );
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum HeaderBinding {
    Literal(String),
    Environment(EnvRef),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub endpoint: String,
    pub api_key: EnvRef,
    #[serde(default)]
    pub headers: BTreeMap<String, HeaderBinding>,
    #[serde(default = "request_timeout")]
    pub timeout_ms: u64,
}
fn request_timeout() -> u64 {
    120_000
}
fn lease_timeout() -> u64 {
    300_000
}
fn docker_host() -> String {
    "unix:///var/run/docker.sock".into()
}
fn docker_binary() -> PathBuf {
    "docker".into()
}
fn memory_mb() -> u64 {
    512
}
fn cpus() -> u32 {
    2
}
fn pids_limit() -> u32 {
    128
}
fn network() -> String {
    "none".into()
}

impl Connection {
    pub fn validate(&self) -> Result<()> {
        validate_url(&self.endpoint, false)?;
        let url = reqwest::Url::parse(&self.endpoint)?;
        ensure!(
            url.path() != "/" && !url.path().is_empty(),
            "model endpoint must include the complete Responses route"
        );
        self.api_key.validate()?;
        ensure!(
            (1_000..=600_000).contains(&self.timeout_ms),
            "model timeout must be 1..600 seconds"
        );
        let mut names = std::collections::BTreeSet::new();
        for (name, value) in &self.headers {
            let name = header_name(name)?;
            ensure!(
                names.insert(name.as_str().to_owned()),
                "duplicate provider header"
            );
            match value {
                HeaderBinding::Literal(value) => {
                    ensure!(
                        !sensitive_header(name.as_str()),
                        "sensitive provider headers require environment references"
                    );
                    HeaderValue::from_str(value).context("invalid provider header value")?;
                }
                HeaderBinding::Environment(handle) => handle.validate()?,
            }
        }
        Ok(())
    }

    pub fn resolve(&self) -> Result<ModelConfig> {
        self.validate()?;
        let mut headers = HeaderMap::new();
        for (name, value) in &self.headers {
            let value = match value {
                HeaderBinding::Literal(value) => value.clone(),
                HeaderBinding::Environment(handle) => handle.resolve()?,
            };
            let mut value =
                HeaderValue::from_str(&value).context("invalid provider header value")?;
            value.set_sensitive(true);
            headers.insert(header_name(name)?, value);
        }
        Ok(ModelConfig {
            redacted_values: Vec::new(),
            endpoint: self.endpoint.clone(),
            api_key: self.api_key.resolve()?,
            headers,
            request_timeout: Duration::from_millis(self.timeout_ms),
        })
    }
}

fn header_name(name: &str) -> Result<HeaderName> {
    let name = HeaderName::from_bytes(name.as_bytes()).context("invalid provider header name")?;
    ensure!(
        !matches!(
            name.as_str(),
            "authorization"
                | "proxy-authorization"
                | "host"
                | "content-length"
                | "content-type"
                | "transfer-encoding"
                | "connection"
                | "accept"
        ),
        "reserved provider header"
    );
    Ok(name)
}

fn sensitive_header(name: &str) -> bool {
    [
        "password",
        "passwd",
        "secret",
        "token",
        "api-key",
        "api_key",
        "apikey",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|part| name.contains(part))
}

pub fn validate_url(value: &str, local: bool) -> Result<()> {
    let url = reqwest::Url::parse(value).context("invalid configured endpoint")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "endpoint must be HTTP(S), without credentials, query or fragment"
    );
    if local {
        ensure!(
            url.host_str().is_some_and(|h| h == "localhost"
                || h.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())),
            "local runtime endpoint must be loopback"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DockerBinding {
    #[serde(default = "docker_host")]
    pub host: String,
    #[serde(default = "docker_binary")]
    pub binary: PathBuf,
    #[serde(default = "memory_mb")]
    pub memory_mb: u64,
    #[serde(default = "cpus")]
    pub cpus: u32,
    #[serde(default = "pids_limit")]
    pub pids_limit: u32,
    #[serde(default = "network")]
    pub network: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEnvBinding {
    pub mode: RuntimeMode,
    pub api_url: Option<String>,
    pub sandbox_url: Option<String>,
    pub api_key: EnvRef,
    #[serde(default = "lease_timeout")]
    pub lease_ms: u64,
}

/// Variant is inferred from the bundle and checked; deployment never chooses a provider.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RuntimeBinding {
    Docker(DockerBinding),
    AgentEnv(AgentEnvBinding),
}

impl RuntimeBinding {
    pub fn validate(&self, bundle: &Bundle) -> Result<()> {
        match self {
            Self::Docker(binding) => {
                ensure!(
                    bundle.manifest.runtime.provider() == "docker",
                    "runtime binding does not match bundle provider"
                );
                for reference in bundle.manifest.runtime.references() {
                    binding.config(reference, "validation").validate()?;
                }
            }
            Self::AgentEnv(binding) => {
                ensure!(
                    bundle.manifest.runtime.provider() == "agentenv",
                    "runtime binding does not match bundle provider"
                );
                let local = binding.mode == RuntimeMode::Local;
                if let Some(url) = &binding.api_url {
                    validate_url(url, local)?;
                } else {
                    ensure!(local, "remote AgentENV runtime requires api_url");
                }
                if let Some(url) = &binding.sandbox_url {
                    validate_url(url, local)?;
                }
                binding.api_key.validate()?;
                ensure!(
                    (30_000..=86_400_000).contains(&binding.lease_ms),
                    "AgentENV lease must be 30 seconds..24 hours"
                );
            }
        }
        Ok(())
    }

    pub fn resolve(
        &self,
        bundle: &Bundle,
        owner: &str,
        image_id: Option<&str>,
    ) -> Result<BackendConfig> {
        self.resolve_for(bundle, owner, bundle.manifest.runtime.reference(), image_id)
    }

    pub fn resolve_for(
        &self,
        bundle: &Bundle,
        owner: &str,
        reference: &str,
        image_id: Option<&str>,
    ) -> Result<BackendConfig> {
        self.validate(bundle)?;
        ensure!(
            bundle.manifest.runtime.references().contains(&reference),
            "runtime reference is not in the Agentfile allowlist"
        );
        match self {
            Self::Docker(binding) => Ok(BackendConfig::Docker(
                binding.config(image_id.unwrap_or(reference), owner),
            )),
            Self::AgentEnv(binding) => Ok(BackendConfig::AgentEnv(RuntimeConfig {
                mode: binding.mode,
                api_url: binding.api_url.clone(),
                sandbox_url: binding.sandbox_url.clone(),
                api_key: binding.api_key.resolve()?,
                template: reference.into(),
                timeout_ms: binding.lease_ms,
            })),
        }
    }
}

impl DockerBinding {
    fn config(&self, image: &str, owner: &str) -> DockerConfig {
        DockerConfig {
            image: image.into(),
            owner: owner.into(),
            host: self.host.clone(),
            binary: self.binary.clone(),
            memory_mb: self.memory_mb,
            cpus: self.cpus,
            pids_limit: self.pids_limit,
            network: self.network.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub id: String,
    pub bundle: PathBuf,
    pub runtime: RuntimeBinding,
    #[serde(default)]
    pub secrets: BTreeMap<String, EnvRef>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub connections: BTreeMap<String, Connection>,
    pub agents: Vec<AgentConfig>,
}

/// Fully selected, public configuration. Safe to persist in the immutable release.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub agent_id: String,
    pub bundle: Bundle,
    pub connection: Connection,
    pub runtime: RuntimeBinding,
    pub secrets: BTreeMap<String, EnvRef>,
}

pub fn validate_identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')),
        "invalid agent or connection identifier"
    );
    Ok(())
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.agent_id)?;
        crate::build::verify_bundle(&self.bundle)?;
        self.connection.validate()?;
        self.runtime.validate(&self.bundle)?;
        ensure!(
            self.secrets.len() == self.bundle.manifest.secrets.len()
                && self
                    .bundle
                    .manifest
                    .secrets
                    .iter()
                    .all(|name| self.secrets.contains_key(name)),
            "secrets must bind exactly the bundle's referenced secrets"
        );
        for handle in self.secrets.values() {
            handle.validate()?;
        }
        Ok(())
    }

    pub fn resolve_secrets(&self) -> Result<BTreeMap<String, String>> {
        self.secrets
            .iter()
            .map(|(name, handle)| Ok((name.clone(), handle.resolve()?)))
            .collect()
    }
}

impl Deployment {
    pub fn read(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .context("cannot open deployment configuration")?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "deployment configuration exceeds 1 MiB"
        );
        // Parser errors may quote secret-looking literals; never expose the source.
        let text = std::str::from_utf8(&bytes).context("deployment must be UTF-8")?;
        let config: Self = toml::from_str(text)
            .map_err(|_| anyhow::anyhow!("invalid deployment TOML or unknown field"))?;
        ensure!(
            !config.connections.is_empty(),
            "configure at least one model connection"
        );
        ensure!(
            (1..=64).contains(&config.agents.len()),
            "configure 1..64 agent profiles"
        );
        for (name, connection) in &config.connections {
            validate_identifier(name)?;
            connection.validate()?;
        }
        let mut ids = std::collections::BTreeSet::new();
        for agent in &config.agents {
            validate_identifier(&agent.id)?;
            ensure!(ids.insert(&agent.id), "duplicate agent profile ID");
            ensure!(
                !agent.bundle.as_os_str().is_empty(),
                "bundle path is required"
            );
        }
        Ok(config)
    }

    pub fn profile(&self, config_path: &Path, id: &str) -> Result<Profile> {
        let agent = self
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .context("agent profile is unavailable")?;
        let directory = config_path.parent().unwrap_or(Path::new("."));
        let bundle = read_bundle(&directory.join(&agent.bundle))?;
        let connection = self
            .connections
            .get(&bundle.manifest.model.connection)
            .context("bundle model connection is not configured")?
            .clone();
        let profile = Profile {
            agent_id: id.into(),
            bundle,
            connection,
            runtime: agent.runtime.clone(),
            secrets: agent.secrets.clone(),
        };
        profile.validate()?;
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "[connections.primary]\nendpoint = 'https://example.com/responses'\napi_key = { env = 'PATH' }\n[[agents]]\nid = 'agent'\nbundle = 'agent.json'\n[agents.runtime]\n";

    fn read(text: &str) -> Result<Deployment> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), text)?;
        Deployment::read(file.path())
    }

    #[test]
    fn parses_deployment_without_accessing_referenced_environment() {
        let config = read(&BASE.replace("'PATH'", "'APORTO_TEST_MISSING_NOT_READ'")).unwrap();
        assert!(matches!(
            config.agents[0].runtime,
            RuntimeBinding::Docker(_)
        ));
        assert!(
            serde_json::to_string(&config)
                .unwrap()
                .contains("APORTO_TEST_MISSING_NOT_READ")
        );
    }

    #[test]
    fn rejects_unknown_duplicate_and_legacy_configuration_fields_without_echoing_values() {
        for text in [
            format!("version = 2\n{BASE}"),
            BASE.replace("api_key = { env = 'PATH' }", "key_env = 'PATH'"),
            format!("{BASE}provider = 'docker'\n"),
            format!(
                "{BASE}mode = 'local'\nhost = 'unix:///var/run/docker.sock'\napi_key = {{ env = 'PATH' }}"
            ),
            BASE.replace(
                "api_key = { env = 'PATH' }",
                "api_key = 'private-key-must-not-be-echoed'",
            ),
            BASE.replace("id = 'agent'", "id = 'agent'\nid = 'other'"),
        ] {
            let error = read(&text).unwrap_err().to_string();
            assert!(!error.contains("private-key-must-not-be-echoed"));
        }
    }

    #[test]
    fn rejects_reserved_duplicate_and_secret_literal_headers() {
        for headers in [
            "Authorization = 'x'",
            "Content-Type = 'x'",
            "Transfer-Encoding = 'x'",
            "Accept = 'x'",
            "X-Token = 'private'",
            "X-Api-Key = 'private'",
            "X-Source = 'a'\nx-source = 'b'",
        ] {
            let text = BASE.replace(
                "[[agents]]",
                &format!("[connections.primary.headers]\n{headers}\n[[agents]]"),
            );
            assert!(read(&text).is_err(), "{headers}");
        }
        assert!(
            read(&BASE.replace(
                "[[agents]]",
                "[connections.primary.headers]\nX-Api-Key = { env = 'PATH' }\n[[agents]]"
            ))
            .is_ok()
        );
    }

    #[test]
    fn validates_environment_handles_and_endpoint_components() {
        for env in ["", "9KEY", "KEY-VALUE", "KEY VALUE"] {
            assert!(read(&BASE.replace("'PATH'", &format!("'{env}'"))).is_err());
        }
        for endpoint in [
            "https://example.com",
            "https://user:pass@example.com/responses",
            "https://example.com/responses?key=private",
            "file:///tmp/responses",
        ] {
            assert!(read(&BASE.replace("https://example.com/responses", endpoint)).is_err());
        }
        assert!(validate_url("http://127.0.0.1:8000", true).is_ok());
        assert!(validate_url("http://[::1]:8000", true).is_ok());
        assert!(validate_url("http://example.com:8000", true).is_err());
    }
}
