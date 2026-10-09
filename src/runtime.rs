//! Runtime selection. Each backend owns its transport, processes and lifecycle.
mod agentenv;
mod docker;

pub use agentenv::{AgentEnvRuntime, RuntimeConfig, RuntimeMode, create_runtime, open_runtime};
pub use docker::{DockerConfig, DockerRuntime};

use crate::types::ManagedRuntime;
use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
pub enum BackendConfig {
    AgentEnv(RuntimeConfig),
    Docker(DockerConfig),
}

impl BackendConfig {
    pub fn provider(&self) -> &'static str {
        match self {
            Self::AgentEnv(_) => "agentenv",
            Self::Docker(_) => "docker",
        }
    }
}

/// Reconnect exactly the recorded workspace; never substitute another backend.
pub async fn open_backend(
    config: BackendConfig,
    workspace_id: Option<&str>,
) -> Result<Arc<dyn ManagedRuntime>> {
    match config {
        BackendConfig::AgentEnv(config) => Ok(open_runtime(config, workspace_id).await?),
        BackendConfig::Docker(config) => Ok(DockerRuntime::open(config, workspace_id).await?),
    }
}
