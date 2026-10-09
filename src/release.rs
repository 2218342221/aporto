//! Activation snapshots the executable catalog and public bindings into immutable releases.
use crate::{
    broker::{BrokerCatalog, create_broker_with_cancel},
    deployment::{Deployment, Profile, validate_identifier},
    runtime::{BackendConfig, open_backend},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

const MAX_RELEASE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub verification: RuntimeVerification,
    pub image_id: Option<String>,
    pub daemon_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeVerification {
    DockerContentAndDaemon,
    #[default]
    AgentenvUnverified,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub id: String,
    pub profile: Profile,
    pub catalog: BrokerCatalog,
    pub runtime_identity: RuntimeIdentity,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variants: BTreeMap<String, RuntimeVariant>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeVariant {
    pub catalog: BrokerCatalog,
    pub runtime_identity: RuntimeIdentity,
}

impl Release {
    fn content_id(&self) -> Result<String> {
        // Fields have deterministic serialization (maps are ordered). Identity excludes itself.
        let content = if self.variants.is_empty() {
            serde_json::to_vec(&(&self.profile, &self.catalog, &self.runtime_identity))?
        } else {
            serde_json::to_vec(&(
                &self.profile,
                &self.catalog,
                &self.runtime_identity,
                &self.variants,
            ))?
        };
        Ok(format!("sha256:{:x}", Sha256::digest(content)))
    }

    pub fn verify(&self) -> Result<()> {
        self.profile.validate()?;
        ensure!(
            self.id == self.content_id()?,
            "release content digest mismatch"
        );
        self.catalog.validate(&self.profile.bundle)?;
        self.validate_identity(&self.runtime_identity)?;
        let references = self.profile.bundle.manifest.runtime.references();
        ensure!(
            self.variants.len() == references.len() - 1
                && references
                    .iter()
                    .skip(1)
                    .all(|reference| self.variants.contains_key(*reference)),
            "release variants must match every non-default runtime reference"
        );
        for variant in self.variants.values() {
            variant.catalog.validate(&self.profile.bundle)?;
            self.validate_identity(&variant.runtime_identity)?;
            ensure!(
                variant.runtime_identity.daemon_id == self.runtime_identity.daemon_id,
                "runtime variants must share the pinned deployment daemon"
            );
        }
        Ok(())
    }

    fn validate_identity(&self, identity: &RuntimeIdentity) -> Result<()> {
        match &self.profile.runtime {
            crate::deployment::RuntimeBinding::Docker(_) => {
                ensure!(
                    identity.verification == RuntimeVerification::DockerContentAndDaemon
                        && identity.image_id.as_deref().is_some_and(valid_digest)
                        && identity
                            .daemon_id
                            .as_ref()
                            .is_some_and(|value| !value.is_empty()
                                && value.len() <= 256
                                && !value.chars().any(char::is_control)),
                    "Docker release requires image and daemon identity"
                );
            }
            crate::deployment::RuntimeBinding::AgentEnv(_) => {
                ensure!(
                    identity.verification == RuntimeVerification::AgentenvUnverified
                        && identity.image_id.is_none()
                        && identity.daemon_id.is_none(),
                    "AgentENV identity cannot be represented as Docker identity"
                );
            }
        }
        Ok(())
    }

    /// Pinned daemon, immutable image and namespace apply equally to CLI and Core.
    pub async fn resolve_backend(&self) -> Result<BackendConfig> {
        self.resolve_backend_for(self.profile.bundle.manifest.runtime.reference())
            .await
    }

    fn selected(&self, reference: &str) -> Result<(&BrokerCatalog, &RuntimeIdentity)> {
        if reference == self.profile.bundle.manifest.runtime.reference() {
            return Ok((&self.catalog, &self.runtime_identity));
        }
        let variant = self
            .variants
            .get(reference)
            .context("runtime reference is not in the release allowlist")?;
        Ok((&variant.catalog, &variant.runtime_identity))
    }

    pub fn catalog_for(&self, reference: &str) -> Result<&BrokerCatalog> {
        self.verify()?;
        Ok(self.selected(reference)?.0)
    }

    pub async fn resolve_backend_for(&self, reference: &str) -> Result<BackendConfig> {
        self.verify()?;
        let (_, identity) = self.selected(reference)?;
        let owner = format!(
            "aporto-{}",
            self.id
                .strip_prefix("sha256:")
                .context("invalid release ID")?
        );
        let config = self.profile.runtime.resolve_for(
            &self.profile.bundle,
            &owner,
            reference,
            identity.image_id.as_deref(),
        )?;
        if let BackendConfig::Docker(docker) = &config {
            let (image, daemon) = docker.identity().await?;
            ensure!(
                Some(&image) == identity.image_id.as_ref()
                    && Some(&daemon) == identity.daemon_id.as_ref(),
                "Docker runtime identity differs from the pinned release"
            );
        }
        Ok(config)
    }
}

pub fn default_directory(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("releases")
}

fn valid_digest(id: &str) -> bool {
    id.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn filename(id: &str) -> Result<String> {
    ensure!(valid_digest(id), "invalid release ID");
    Ok(format!("{}.release.json", &id[7..]))
}

fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).context("cannot open release file")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "release entries must be regular files"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "release file exceeds size limit");
    Ok(bytes)
}

pub fn load(directory: &Path, id: &str) -> Result<Release> {
    let bytes = read_limited(&directory.join(filename(id)?), MAX_RELEASE_BYTES)?;
    let release: Release =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid release file"))?;
    ensure!(
        release.id == id,
        "release filename differs from content identity"
    );
    release.verify()?;
    Ok(release)
}

pub fn active(directory: &Path) -> Result<BTreeMap<String, String>> {
    let bytes = read_limited(&directory.join("active.json"), 1024 * 1024)?;
    let map: BTreeMap<String, String> = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid active release index"))?;
    for (agent, release) in &map {
        validate_identifier(agent)?;
        ensure!(valid_digest(release), "invalid active release ID");
    }
    Ok(map)
}

/// Used for new CLI executions. Existing threads load their recorded ID directly.
pub fn load_active(config_path: &Path, agent_id: &str, directory: &Path) -> Result<Release> {
    let deployment = Deployment::read(config_path)?;
    ensure!(
        deployment.agents.iter().any(|agent| agent.id == agent_id),
        "agent profile is unavailable"
    );
    let index = active(directory).context("activate the agent before running it")?;
    let id = index
        .get(agent_id)
        .context("agent has no active release; run activate first")?;
    let release = load(directory, id)?;
    ensure!(
        release.profile.agent_id == agent_id,
        "active release agent mismatch"
    );
    Ok(release)
}

pub fn load_all(directory: &Path) -> Result<BTreeMap<String, Release>> {
    let mut releases = BTreeMap::new();
    for entry in fs::read_dir(directory).context("cannot open release store")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(hash) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".release.json"))
        else {
            continue;
        };
        ensure!(releases.len() < 4096, "release store exceeds 4096 entries");
        let id = format!("sha256:{hash}");
        releases.insert(id.clone(), load(directory, &id)?);
    }
    Ok(releases)
}

fn private(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    Ok(())
}

fn temporary(directory: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let path = directory.join(format!(".publish-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    if let Err(error) = written {
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    Ok(path)
}

fn publish(directory: &Path, release: &Release) -> Result<()> {
    release.verify()?;
    let encoded = serde_json::to_vec(release)?;
    ensure!(
        encoded.len() as u64 <= MAX_RELEASE_BYTES,
        "release file exceeds size limit"
    );
    if let Ok(metadata) = fs::symlink_metadata(directory) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "release store must be a directory, not a symlink"
        );
    }
    fs::create_dir_all(directory)?;
    private(directory, true)?;
    let lock_path = directory.join("activation.lock");
    if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "invalid activation lock"
        );
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock_exclusive()?;
    let destination = directory.join(filename(&release.id)?);
    if destination.exists() {
        load(directory, &release.id)?;
    } else {
        let temp = temporary(directory, &encoded)?;
        let result = fs::hard_link(&temp, destination);
        let _ = fs::remove_file(temp);
        result.context("cannot publish immutable release")?;
        File::open(directory)?.sync_all()?;
    }
    let mut index = if directory.join("active.json").exists() {
        active(directory)?
    } else {
        BTreeMap::new()
    };
    index.insert(release.profile.agent_id.clone(), release.id.clone());
    let temp = temporary(directory, &serde_json::to_vec_pretty(&index)?)?;
    let result = fs::rename(&temp, directory.join("active.json"));
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

/// Always attempts runtime deletion, including partial broker setup failures.
async fn discover_and_close(
    profile: &Profile,
    runtime: std::sync::Arc<dyn crate::types::ManagedRuntime>,
    secrets: BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<BrokerCatalog> {
    let runtime_locator = format!(
        "{}:{}",
        profile.bundle.manifest.runtime.provider(),
        runtime.id()
    );
    if cancel.is_cancelled() {
        ensure!(
            runtime.close().await.is_ok(),
            "activation runtime cleanup failed for {runtime_locator}; release was not published"
        );
        anyhow::bail!(
            "activation interrupted; temporary runtime deleted, release was not published"
        );
    }
    let broker = create_broker_with_cancel(
        profile.bundle.clone(),
        runtime.clone(),
        secrets,
        cancel.clone(),
    )
    .await;
    match broker {
        Ok(broker) => {
            let catalog = broker.catalog();
            let tools_closed = broker.close().await;
            let runtime_closed = runtime.close().await;
            ensure!(
                runtime_closed.is_ok(),
                "activation runtime cleanup failed for {runtime_locator}; release was not published"
            );
            ensure!(
                tools_closed.is_ok(),
                "activation tool cleanup failed for {runtime_locator}; runtime deleted, release was not published"
            );
            ensure!(
                !cancel.is_cancelled(),
                "activation interrupted; temporary runtime deleted, release was not published"
            );
            Ok(catalog)
        }
        Err(_) => {
            ensure!(
                runtime.close().await.is_ok(),
                "activation runtime cleanup failed for {runtime_locator}; release was not published"
            );
            ensure!(
                !cancel.is_cancelled(),
                "activation interrupted; temporary runtime deleted, release was not published"
            );
            anyhow::bail!(
                "activation tool discovery failed; temporary runtime deleted, release was not published"
            )
        }
    }
}

/// Activation performs no model request. It verifies environment handles, starts an
/// isolated runtime, snapshots tool contracts, then cleans up before publishing.
pub async fn activate(config_path: &Path, agent_id: &str, directory: &Path) -> Result<Release> {
    activate_with_cancel(config_path, agent_id, directory, CancellationToken::new()).await
}

/// Signal-aware activation. Cancellation stops discovery/publication while still
/// awaiting create and cleanup, so a runtime ID is never abandoned in flight.
pub async fn activate_with_cancel(
    config_path: &Path,
    agent_id: &str,
    directory: &Path,
    cancel: CancellationToken,
) -> Result<Release> {
    ensure!(
        !cancel.is_cancelled(),
        "activation interrupted before runtime allocation"
    );
    let deployment = Deployment::read(config_path)?;
    let profile = deployment.profile(config_path, agent_id)?;
    // Verify referenced operator bindings before starting any guest.
    let _model = profile.connection.resolve()?;
    let secrets = profile.resolve_secrets()?;
    activate_profile(
        profile,
        secrets,
        directory.to_path_buf(),
        cancel,
        Arc::new(ProductionLauncher),
    )
    .await
}

#[async_trait::async_trait]
trait RuntimeLauncher: Send + Sync {
    async fn open(
        &self,
        profile: &Profile,
        owner: &str,
        reference: &str,
    ) -> Result<(Arc<dyn crate::types::ManagedRuntime>, RuntimeIdentity)>;
}

struct ProductionLauncher;

#[async_trait::async_trait]
impl RuntimeLauncher for ProductionLauncher {
    async fn open(
        &self,
        profile: &Profile,
        owner: &str,
        reference: &str,
    ) -> Result<(Arc<dyn crate::types::ManagedRuntime>, RuntimeIdentity)> {
        let mut config = profile
            .runtime
            .resolve_for(&profile.bundle, owner, reference, None)?;
        let mut identity = RuntimeIdentity::default();
        if let BackendConfig::Docker(docker) = &mut config {
            let (image_id, daemon_id) = docker.identity().await?;
            docker.image = image_id.clone();
            identity = RuntimeIdentity {
                verification: RuntimeVerification::DockerContentAndDaemon,
                image_id: Some(image_id),
                daemon_id: Some(daemon_id),
            };
        }
        Ok((open_backend(config, None).await?, identity))
    }
}

async fn activate_profile(
    profile: Profile,
    secrets: BTreeMap<String, String>,
    directory: PathBuf,
    cancel: CancellationToken,
    launcher: Arc<dyn RuntimeLauncher>,
) -> Result<Release> {
    let owner = format!("aporto-activate-{}", uuid::Uuid::new_v4());
    // Supervisor guarantees waiter cancellation still completes runtime cleanup.
    let (mut tx, rx) = tokio::sync::oneshot::channel();
    let cancel = cancel.child_token();
    tokio::spawn(async move {
        let result = async {
            let mut variants = BTreeMap::new();
            let mut default = None;
            for reference in profile.bundle.manifest.runtime.references() {
                ensure!(
                    !cancel.is_cancelled() && !tx.is_closed(),
                    "activation interrupted before runtime allocation"
                );
                // Await allocation even after cancellation so an allocated ID is
                // always returned to the cleanup path.
                let (runtime, runtime_identity) =
                    launcher.open(&profile, &owner, reference).await?;
                if tx.is_closed() {
                    cancel.cancel();
                }
                let discovery =
                    discover_and_close(&profile, runtime, secrets.clone(), cancel.clone());
                tokio::pin!(discovery);
                let catalog = tokio::select! {
                    result = &mut discovery => result?,
                    _ = tx.closed() => {
                        cancel.cancel();
                        discovery.await?
                    }
                };
                let variant = RuntimeVariant {
                    catalog,
                    runtime_identity,
                };
                if default.is_none() {
                    default = Some(variant);
                } else {
                    variants.insert(reference.to_owned(), variant);
                }
            }
            let default = default.context("runtime has no default reference")?;
            let mut release = Release {
                id: String::new(),
                profile,
                catalog: default.catalog,
                runtime_identity: default.runtime_identity,
                variants,
            };
            release.id = release.content_id()?;
            // A dropped caller never changes the active release after it leaves.
            ensure!(
                !tx.is_closed(),
                "activation caller disconnected before publication"
            );
            ensure!(
                !cancel.is_cancelled(),
                "activation interrupted before publication"
            );
            publish(&directory, &release)?;
            Ok(release)
        }
        .await;
        let _ = tx.send(result);
    });
    rx.await.context("activation supervisor stopped")?
}

#[cfg(test)]
mod tests;
