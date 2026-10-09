//! Executes threads against their immutable activated release.
use crate::{EventSink, ExecutionContext, ExecutionResult, TurnExecutor};
use anyhow::{Context, Result, bail, ensure};
use aporto::{
    broker::create_broker_in_workspace,
    deployment::Deployment,
    model::{RunObserver, initial_input, run_agent_turn},
    release::{self, Release},
    runtime::{BackendConfig, open_backend},
};
use aporto_protocol::{AgentSummary, RuntimeOptions};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

/// Both current and historical releases are loaded as immutable snapshots. Operator
/// file edits affect future activation only; existing threads retain their bindings.
pub struct ProductionExecutor {
    agents: Vec<AgentSummary>,
    releases: BTreeMap<String, Release>,
}

impl ProductionExecutor {
    pub fn from_config_file(path: &Path) -> Result<Self> {
        Self::from_config_file_and_releases(path, &release::default_directory(path))
    }

    pub fn from_config_file_and_releases(path: &Path, directory: &Path) -> Result<Self> {
        let config = Deployment::read(path)?;
        let index = release::active(directory).context("activate profiles before starting Core")?;
        let releases = release::load_all(directory)?;
        let mut agents = Vec::new();
        for agent in config.agents {
            let id = index
                .get(&agent.id)
                .context("configured agent has no active release")?;
            let release = releases.get(id).context("active release is missing")?;
            ensure!(
                release.profile.agent_id == agent.id,
                "active release agent mismatch"
            );
            agents.push(AgentSummary {
                id: agent.id,
                name: release.profile.bundle.manifest.name.clone(),
                model: release.profile.bundle.manifest.model.name.clone(),
                bundle_digest: release.profile.bundle.digest.clone(),
                release_id: id.clone(),
                runtime: runtime_options(release),
            });
        }
        Ok(Self { agents, releases })
    }
}

fn runtime_options(release: &Release) -> RuntimeOptions {
    let runtime = &release.profile.bundle.manifest.runtime;
    RuntimeOptions {
        provider: runtime.provider().into(),
        images: runtime
            .references()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        default_image: runtime.reference().into(),
        default_workdir: runtime.workdir().into(),
    }
}

struct Observer(EventSink);
#[async_trait]
impl RunObserver for Observer {
    async fn emit(&self, kind: &str, data: Value) -> Result<()> {
        self.0.emit(kind, data).await
    }
}

#[async_trait]
impl TurnExecutor for ProductionExecutor {
    fn agents(&self) -> Vec<AgentSummary> {
        self.agents.clone()
    }

    fn runtime_options(
        &self,
        agent_id: &str,
        release_id: &str,
        bundle_digest: &str,
    ) -> Option<RuntimeOptions> {
        self.releases
            .get(release_id)
            .filter(|release| {
                release.profile.agent_id == agent_id
                    && release.profile.bundle.digest == bundle_digest
            })
            .map(runtime_options)
    }

    fn supports_release(&self, agent_id: &str, release_id: &str, bundle_digest: &str) -> bool {
        self.releases.get(release_id).is_some_and(|release| {
            release.profile.agent_id == agent_id && release.profile.bundle.digest == bundle_digest
        })
    }

    async fn execute(&self, mut ctx: ExecutionContext) -> Result<ExecutionResult> {
        let release = self
            .releases
            .get(&ctx.release_id)
            .context("thread release is unavailable")?;
        ensure!(
            release.profile.agent_id == ctx.agent_id
                && release.profile.bundle.digest == ctx.bundle_digest,
            "thread release binding is invalid"
        );
        ensure!(
            !ctx.cancel.is_cancelled(),
            "turn interrupted before runtime resolution"
        );
        let profile = &release.profile;
        ensure!(
            ctx.runtime_provider == profile.bundle.manifest.runtime.provider(),
            "thread runtime provider mismatch"
        );
        let workdir = aporto::workspace::validate_workdir(&ctx.workdir)?;
        let runtime_config = release.resolve_backend_for(&ctx.runtime_image).await?;
        let mut model = profile.connection.resolve()?;
        let secrets = profile.resolve_secrets()?;
        model
            .redacted_values
            .extend(secrets.values().cloned().chain(match &runtime_config {
                BackendConfig::AgentEnv(config) => Some(config.api_key.clone()),
                BackendConfig::Docker(_) => None,
            }));
        ensure!(
            !ctx.cancel.is_cancelled(),
            "turn interrupted before runtime allocation"
        );
        // Do not cancel a create request halfway through and lose a known sandbox ID.
        // Backend-specific cleanup applies to unknown outcomes; Docker has no lease.
        let runtime = match open_backend(runtime_config.clone(), ctx.sandbox_id.as_deref()).await {
            Ok(runtime) => runtime,
            Err(_) => {
                ctx.events
                    .emit(
                        "runtime.failed",
                        json!({"stage":"connect","reason":"runtime_unavailable"}),
                    )
                    .await?;
                match &runtime_config {
                    BackendConfig::AgentEnv(_) => bail!(
                        "AgentENV create/connect failed; verify endpoint, credentials, template and sandbox availability"
                    ),
                    BackendConfig::Docker(_) => bail!(
                        "Docker create/start failed; verify daemon access, local image and recorded container ownership"
                    ),
                }
            }
        };
        if let Err(error) = ctx
            .events
            .emit("runtime.ready", json!({"sandbox_id":runtime.id()}))
            .await
        {
            if ctx.sandbox_id.is_none() {
                let _ = runtime.close().await;
            } else {
                let _ = runtime.pause().await;
            }
            return Err(error);
        }
        let execution = async {
            ensure!(
                !ctx.cancel.is_cancelled(),
                "turn interrupted during runtime allocation"
            );
            let setup = create_broker_in_workspace(
                profile.bundle.clone(),
                runtime.clone(),
                secrets,
                &workdir,
                ctx.cancel.clone(),
            )
            .await;
            let broker = match setup {
                Ok(mut broker) => {
                    if broker
                        .enforce_catalog(release.catalog_for(&ctx.runtime_image)?)
                        .is_err()
                    {
                        let _ = broker.close().await;
                        bail!("packaged tool contract differs from the pinned release");
                    }
                    Arc::new(broker)
                }
                Err(_) => {
                    ctx.events
                        .emit(
                            "tool.initialization_failed",
                            json!({"reason":"packaged_tool_unavailable"}),
                        )
                        .await?;
                    bail!("packaged tools failed to initialize");
                }
            };
            if let Some(note) = ctx.recovery_note.take() {
                if ctx.history.is_empty() {
                    ctx.history = initial_input(&profile.bundle, &ctx.input)?;
                    ctx.history.pop(); // run_agent_turn appends the current user input once.
                }
                ctx.history.push(json!({"role":"developer","content":note}));
            }
            let result = run_agent_turn(
                &profile.bundle,
                broker.clone(),
                model,
                &ctx.input,
                ctx.history,
                Arc::new(Observer(ctx.events.clone())),
                ctx.cancel.clone(),
            )
            .await;
            let cleanup = broker.close().await;
            if result.is_err() {
                ctx.events
                    .emit("model.failed", json!({"reason":"execution_error"}))
                    .await?;
            }
            if cleanup.is_err() {
                ctx.events
                    .emit("tool.cleanup_failed", json!({"reason":"mcp_cleanup_error"}))
                    .await?;
            }
            let result = result.map_err(|_| {
                anyhow::anyhow!("model/PTC execution failed; inspect the workspace before retrying")
            })?;
            ensure!(
                cleanup.is_ok(),
                "MCP cleanup failed; inspect the sandbox before retrying"
            );
            Ok::<_, anyhow::Error>(ExecutionResult {
                answer: result.run.answer,
                history: result.history,
            })
        }
        .await;
        // TODO: Hand off the live runtime through the instance scheduler when another
        // turn is queued for this instance, avoiding a stop/start cycle while keeping
        // tool cleanup and execution serialized.
        let paused = runtime.pause().await;
        ctx.events
            .emit(
                "runtime.paused",
                json!({"sandbox_id":runtime.id(),"confirmed":paused.is_ok()}),
            )
            .await?;
        if paused.is_err() && execution.is_ok() {
            match &runtime_config {
                BackendConfig::AgentEnv(_) => bail!(
                    "AgentENV pause failed; workspace ID retained, lease expiration will attempt autoPause"
                ),
                BackendConfig::Docker(_) => bail!(
                    "Docker stop failed; container ID retained, inspect the container because no automatic pause lease exists"
                ),
            }
        }
        execution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aporto::{
        broker::{BrokerCatalog, ToolContract, ToolOrigin, builtin_definitions},
        release::RuntimeIdentity,
    };
    use sha2::{Digest, Sha256};

    fn save_release(directory: &Path, release: &mut Release) {
        release.id = format!(
            "sha256:{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    &release.profile,
                    &release.catalog,
                    &release.runtime_identity
                ))
                .unwrap()
            )
        );
        release.verify().unwrap();
        std::fs::write(
            directory.join(format!("{}.release.json", &release.id[7..])),
            serde_json::to_vec(release).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn production_profiles_preserve_removed_agents_and_deleted_source_bundles() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("Agentfile"), "[agent]\nname='Test'\n[model]\nname='test'\nconnection='primary'\n[runtime]\nprovider='agentenv'\ntemplate='test-template'\n").unwrap();
        let bundle = aporto::build::build(root, Path::new("Agentfile"), &BTreeMap::new()).unwrap();
        std::fs::write(
            root.join("bundle.json"),
            serde_json::to_vec(&bundle).unwrap(),
        )
        .unwrap();
        let config_path = root.join("deployment.toml");
        let config = "[connections.primary]\nendpoint='https://old.example.com/responses'\napi_key={env='PATH'}\n[[agents]]\nid='old'\nbundle='bundle.json'\n[agents.runtime]\nmode='local'\napi_key={env='PATH'}\n";
        std::fs::write(&config_path, config).unwrap();
        let profile = Deployment::read(&config_path)
            .unwrap()
            .profile(&config_path, "old")
            .unwrap();
        let catalog = BrokerCatalog {
            builtin_abi: bundle.manifest.builtin_abi.clone(),
            ptc_abi: bundle.manifest.ptc_abi.clone(),
            tools: builtin_definitions()
                .into_iter()
                .map(|definition| {
                    (
                        definition.name.clone(),
                        ToolContract {
                            definition,
                            origin: ToolOrigin::Builtin,
                            output_schema: None,
                            annotations: None,
                        },
                    )
                })
                .collect(),
        };
        let mut old = Release {
            id: String::new(),
            profile,
            catalog,
            runtime_identity: RuntimeIdentity::default(),
            variants: Default::default(),
        };
        let releases = root.join("releases");
        std::fs::create_dir(&releases).unwrap();
        save_release(&releases, &mut old);
        let mut current = old.clone();
        current.profile.agent_id = "new".into();
        current.profile.connection.endpoint = "https://new.example.com/responses".into();
        save_release(&releases, &mut current);
        std::fs::write(
            releases.join("active.json"),
            serde_json::to_vec(&json!({"new":current.id})).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &config_path,
            config
                .replace("id='old'", "id='new'")
                .replace("old.example.com", "edited-but-not-activated.example.com"),
        )
        .unwrap();
        std::fs::remove_file(root.join("bundle.json")).unwrap();
        let executor = ProductionExecutor::from_config_file(&config_path).unwrap();
        assert_eq!(executor.agents()[0].id, "new");
        assert!(executor.supports_release("old", &old.id, &bundle.digest));
        assert!(!executor.supports_release("new", &old.id, &bundle.digest));
        assert_eq!(
            executor.releases[&old.id].profile.connection.endpoint,
            "https://old.example.com/responses"
        );
        assert_eq!(
            executor.releases[&current.id].profile.connection.endpoint,
            "https://new.example.com/responses"
        );
    }
}
