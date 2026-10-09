use super::*;
use crate::broker::{ToolContract, ToolOrigin, builtin_definitions};

fn fixture() -> (tempfile::TempDir, PathBuf, Release) {
    fixture_with("provider='agentenv'\ntemplate='test-template'", "")
}

fn fixture_with(runtime: &str, extra: &str) -> (tempfile::TempDir, PathBuf, Release) {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("Agentfile"), format!("[agent]\nname='Test'\n[model]\nname='test-model'\nconnection='primary'\n[runtime]\n{runtime}\n{extra}\n")).unwrap();
    let bundle =
        crate::build::build(root.path(), Path::new("Agentfile"), &BTreeMap::new()).unwrap();
    fs::write(
        root.path().join("bundle.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    let path = root.path().join("deployment.toml");
    let binding = if bundle.manifest.runtime.provider() == "agentenv" {
        "mode='local'\napi_key={env='PATH'}\n"
    } else {
        ""
    };
    fs::write(&path, format!("[connections.primary]\nendpoint='https://example.com/responses'\napi_key={{env='PATH'}}\n[[agents]]\nid='test'\nbundle='bundle.json'\n[agents.runtime]\n{binding}")).unwrap();
    let profile = Deployment::read(&path)
        .unwrap()
        .profile(&path, "test")
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
    let variants = bundle
        .manifest
        .runtime
        .references()
        .into_iter()
        .skip(1)
        .map(|reference| {
            (
                reference.to_owned(),
                RuntimeVariant {
                    catalog: catalog.clone(),
                    runtime_identity: RuntimeIdentity::default(),
                },
            )
        })
        .collect();
    let mut release = Release {
        id: String::new(),
        profile,
        catalog,
        runtime_identity: RuntimeIdentity::default(),
        variants,
    };
    release.id = release.content_id().unwrap();
    (root, path, release)
}

#[test]
fn publishing_changed_bindings_preserves_old_release_and_embedded_bundle() {
    let (root, config, old) = fixture();
    let directory = root.path().join("releases");
    publish(&directory, &old).unwrap();
    let mut next = old.clone();
    next.profile.connection.endpoint = "https://new.example.com/responses".into();
    next.id = next.content_id().unwrap();
    assert_ne!(old.id, next.id);
    publish(&directory, &next).unwrap();
    fs::remove_file(root.path().join("bundle.json")).unwrap();
    assert_eq!(
        load_active(&config, "test", &directory).unwrap().id,
        next.id
    );
    let loaded = load(&directory, &old.id).unwrap();
    assert_eq!(
        loaded.profile.connection.endpoint,
        "https://example.com/responses"
    );
    assert_eq!(loaded.profile.bundle.digest, old.profile.bundle.digest);
    assert_eq!(load_all(&directory).unwrap().len(), 2);
}

#[test]
fn release_digest_covers_catalog_runtime_binding_and_environment_handles() {
    let (_, _, release) = fixture();
    for change in 0..3 {
        let mut next = release.clone();
        match change {
            0 => next
                .catalog
                .tools
                .get_mut("exec_command")
                .unwrap()
                .definition
                .description
                .push('!'),
            1 => next.profile.connection.api_key.env = "ANOTHER_MODEL_ACCOUNT".into(),
            _ => match &mut next.profile.runtime {
                crate::deployment::RuntimeBinding::AgentEnv(binding) => {
                    binding.api_url = Some("http://127.0.0.1:9000".into())
                }
                _ => unreachable!(),
            },
        }
        assert!(next.verify().is_err());
        assert_ne!(next.content_id().unwrap(), release.id);
    }
    let encoded = serde_json::to_string(&release).unwrap();
    assert!(encoded.contains("\"env\":\"PATH\""));
    assert!(!encoded.contains(&std::env::var("PATH").unwrap()));
    assert!(encoded.contains("agentenv_unverified"));
}

#[test]
fn resigning_invalid_content_does_not_bypass_semantic_validation() {
    let (_, _, mut release) = fixture();
    release.profile.connection.endpoint = "https://example.com/responses?secret=value".into();
    release.id = release.content_id().unwrap();
    assert!(release.verify().is_err());
    let (_, _, mut release) = fixture();
    release.catalog.tools.remove("exec_command");
    release.id = release.content_id().unwrap();
    assert!(release.verify().is_err());
}

#[test]
fn invalid_active_index_does_not_replace_previously_published_release() {
    let (root, _, release) = fixture();
    let directory = root.path().join("releases");
    publish(&directory, &release).unwrap();
    fs::write(directory.join("active.json"), b"invalid index").unwrap();
    assert!(publish(&directory, &release).is_err());
    assert_eq!(load(&directory, &release.id).unwrap().id, release.id);
    assert_eq!(
        fs::read(directory.join("active.json")).unwrap(),
        b"invalid index"
    );
}

struct TestRuntime {
    fail_setup: bool,
    fail_cleanup: bool,
    cancel_on_exec: Option<CancellationToken>,
    deleted: std::sync::atomic::AtomicBool,
    record: Option<(String, Arc<std::sync::Mutex<Vec<String>>>)>,
    closed_signal: Option<CancellationToken>,
}

#[async_trait::async_trait]
impl crate::types::Runtime for TestRuntime {
    fn id(&self) -> &str {
        self.record
            .as_ref()
            .map(|(name, _)| name.as_str())
            .unwrap_or("activation-fixture")
    }
    async fn exec(
        &self,
        _: &str,
        _: crate::types::ExecOptions,
    ) -> Result<crate::types::CommandResult> {
        if let Some(cancel) = &self.cancel_on_exec {
            cancel.cancel();
            std::future::pending::<()>().await;
        }
        ensure!(!self.fail_setup, "private runtime error must not escape");
        Ok(crate::types::CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        })
    }
    async fn start_process(
        &self,
        _: &[String],
        _: crate::types::ExecOptions,
    ) -> Result<Box<dyn crate::types::RuntimeProcess>> {
        anyhow::bail!("unused")
    }
    async fn read_file(&self, _: &str) -> Result<Vec<u8>> {
        anyhow::bail!("unused")
    }
    async fn write_file(&self, _: &str, _: &[u8]) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.deleted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some((reference, events)) = &self.record {
            events.lock().unwrap().push(format!("close:{reference}"));
        }
        if let Some(closed) = &self.closed_signal {
            closed.cancel();
        }
        ensure!(!self.fail_cleanup, "private cleanup error");
        Ok(())
    }
}
#[async_trait::async_trait]
impl crate::types::ManagedRuntime for TestRuntime {
    async fn pause(&self) -> Result<()> {
        anyhow::bail!("activation must delete, not pause")
    }
}

#[tokio::test]
async fn activation_discovery_and_cleanup_failures_cannot_reach_publication() {
    for (fail_setup, fail_cleanup) in [(false, false), (true, false), (false, true), (true, true)] {
        let (root, _, release) = fixture();
        let runtime = std::sync::Arc::new(TestRuntime {
            fail_setup,
            fail_cleanup,
            cancel_on_exec: None,
            deleted: Default::default(),
            record: None,
            closed_signal: None,
        });
        let result = discover_and_close(
            &release.profile,
            runtime.clone(),
            BTreeMap::new(),
            CancellationToken::new(),
        )
        .await;
        assert!(runtime.deleted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!root.path().join("releases").exists());
        if fail_setup || fail_cleanup {
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("private"));
            if fail_cleanup {
                assert!(error.contains("agentenv:activation-fixture"));
            }
        } else {
            result.unwrap().validate(&release.profile.bundle).unwrap();
        }
    }
}

#[tokio::test]
async fn activation_cancellation_deletes_runtime_before_returning() {
    for cancel_before_setup in [true, false] {
        let (root, _, release) = fixture();
        let cancel = CancellationToken::new();
        if cancel_before_setup {
            cancel.cancel();
        }
        let runtime = std::sync::Arc::new(TestRuntime {
            fail_setup: false,
            fail_cleanup: false,
            cancel_on_exec: (!cancel_before_setup).then(|| cancel.clone()),
            deleted: Default::default(),
            record: None,
            closed_signal: None,
        });
        let error = discover_and_close(&release.profile, runtime.clone(), BTreeMap::new(), cancel)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("interrupted"));
        assert!(runtime.deleted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!root.path().join("releases").exists());
    }
}

#[test]
fn omitted_runtime_options_preserve_single_choice_release_bytes_and_digest() {
    let (_root, _, release) = fixture();
    let encoded = serde_json::to_vec(&release).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert!(value.get("variants").is_none());
    assert_eq!(
        value["profile"]["bundle"]["manifest"]["runtime"],
        serde_json::json!({"provider":"agentenv","template":"test-template"})
    );
    let content = serde_json::to_vec(&(
        &release.profile,
        &release.catalog,
        &release.runtime_identity,
    ))
    .unwrap();
    assert_eq!(release.id, format!("sha256:{:x}", Sha256::digest(content)));
    let decoded: Release = serde_json::from_slice(&encoded).unwrap();
    decoded.verify().unwrap();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
    let (_other, _, explicit) = fixture_with(
        "provider='agentenv'\ntemplate='test-template'\ntemplates=['test-template']\nworkdir='/workspace/'",
        "",
    );
    assert_eq!(explicit.id, release.id);
}

#[test]
fn every_runtime_variant_catalog_and_identity_is_covered_and_validated() {
    let (_root, _, mut release) = fixture_with(
        "provider='agentenv'\ntemplate='test-template'\ntemplates=['other','third']",
        "[[mcp]]\nname='policy'\nurl='https://example.com/mcp'",
    );
    release
        .variants
        .get_mut("other")
        .unwrap()
        .catalog
        .tools
        .insert(
            "mcp__policy__alternate".into(),
            ToolContract {
                definition: crate::types::ToolDefinition {
                    name: "mcp__policy__alternate".into(),
                    description: "available in other template".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                    parallel: false,
                },
                origin: ToolOrigin::Mcp {
                    server: "policy".into(),
                    name: "alternate".into(),
                    protocol: "2025-06-18".into(),
                },
                output_schema: None,
                annotations: None,
            },
        );
    release.id = release.content_id().unwrap();
    release.verify().unwrap();
    assert!(
        !release
            .catalog_for("test-template")
            .unwrap()
            .tools
            .contains_key("mcp__policy__alternate")
    );
    assert!(
        release
            .catalog_for("other")
            .unwrap()
            .tools
            .contains_key("mcp__policy__alternate")
    );
    assert!(release.catalog_for("unlisted").is_err());
    for change in 0..5 {
        let mut altered = release.clone();
        match change {
            0 => {
                altered.variants.remove("other");
            }
            1 => {
                let value = altered.variants.remove("other").unwrap();
                altered.variants.insert("unlisted".into(), value);
            }
            2 => {
                let value = altered.variants.remove("other").unwrap();
                altered.variants.insert("test-template".into(), value);
            }
            3 => {
                altered
                    .variants
                    .get_mut("other")
                    .unwrap()
                    .catalog
                    .tools
                    .remove("exec_command");
            }
            _ => {
                altered
                    .variants
                    .get_mut("other")
                    .unwrap()
                    .runtime_identity
                    .image_id = Some(format!("sha256:{}", "a".repeat(64)));
            }
        }
        assert!(altered.verify().is_err());
        altered.id = altered.content_id().unwrap();
        assert!(
            altered.verify().is_err(),
            "resigned invalid variant {change}"
        );
    }
    let mut changed_contract = release.clone();
    changed_contract
        .variants
        .get_mut("other")
        .unwrap()
        .catalog
        .tools
        .get_mut("mcp__policy__alternate")
        .unwrap()
        .definition
        .description
        .push('!');
    assert_ne!(changed_contract.content_id().unwrap(), release.id);
    assert!(changed_contract.verify().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn docker_variants_resolve_the_selected_pinned_image_and_same_release_owner() {
    use std::os::unix::fs::PermissionsExt;
    let (root, _, mut release) = fixture_with(
        "provider='docker'\nimage='default:latest'\nimages=['alternate:latest']",
        "",
    );
    let binary = root.path().join("docker-fixture");
    fs::write(&binary, "#!/bin/sh\nif [ \"$3\" = image ]; then\n  for last in \"$@\"; do :; done\n  printf '%s\\n' \"$last\"\nelse\n  printf '%s\\n' fixture-daemon\nfi\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let crate::deployment::RuntimeBinding::Docker(binding) = &mut release.profile.runtime else {
        panic!()
    };
    binding.binary = binary.clone();
    let identity = |digit: &str| RuntimeIdentity {
        verification: RuntimeVerification::DockerContentAndDaemon,
        image_id: Some(format!("sha256:{}", digit.repeat(64))),
        daemon_id: Some("fixture-daemon".into()),
    };
    release.runtime_identity = identity("a");
    release
        .variants
        .get_mut("alternate:latest")
        .unwrap()
        .runtime_identity = identity("b");
    release.id = release.content_id().unwrap();
    let BackendConfig::Docker(default) = release.resolve_backend().await.unwrap() else {
        panic!()
    };
    let BackendConfig::Docker(alternate) = release
        .resolve_backend_for("alternate:latest")
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(default.image, format!("sha256:{}", "a".repeat(64)));
    assert_eq!(alternate.image, format!("sha256:{}", "b".repeat(64)));
    assert_eq!(default.owner, alternate.owner);
    assert_eq!(default.owner, format!("aporto-{}", &release.id[7..]));
    assert!(
        release
            .resolve_backend_for("unlisted:latest")
            .await
            .is_err()
    );
    fs::write(binary, "#!/bin/sh\nprintf '%s\\n' wrong-identity\n").unwrap();
    assert!(
        release
            .resolve_backend_for("alternate:latest")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn agentenv_variant_selection_keeps_the_explicit_template_and_connection() {
    let (_root, _, release) = fixture_with(
        "provider='agentenv'\ntemplate='test-template'\ntemplates=['other']",
        "",
    );
    let BackendConfig::AgentEnv(default) = release.resolve_backend().await.unwrap() else {
        panic!()
    };
    let BackendConfig::AgentEnv(other) = release.resolve_backend_for("other").await.unwrap() else {
        panic!()
    };
    assert_eq!(default.template, "test-template");
    assert_eq!(other.template, "other");
    assert_eq!(other.mode, default.mode);
    assert_eq!(other.api_url, default.api_url);
    assert_eq!(other.sandbox_url, default.sandbox_url);
    assert_eq!(other.timeout_ms, default.timeout_ms);
    assert!(release.resolve_backend_for("unlisted").await.is_err());
}

struct FixtureLauncher {
    events: Arc<std::sync::Mutex<Vec<String>>>,
    fail_setup: bool,
    fail_cleanup: bool,
    block_second: bool,
    entered: CancellationToken,
    gate: tokio::sync::Notify,
    closed_second: CancellationToken,
}

impl FixtureLauncher {
    fn new() -> Self {
        Self {
            events: Default::default(),
            fail_setup: false,
            fail_cleanup: false,
            block_second: false,
            entered: CancellationToken::new(),
            gate: tokio::sync::Notify::new(),
            closed_second: CancellationToken::new(),
        }
    }
}

#[async_trait::async_trait]
impl RuntimeLauncher for FixtureLauncher {
    async fn open(
        &self,
        _: &Profile,
        _: &str,
        reference: &str,
    ) -> Result<(Arc<dyn crate::types::ManagedRuntime>, RuntimeIdentity)> {
        self.events
            .lock()
            .unwrap()
            .push(format!("open:{reference}"));
        let second = reference == "other";
        if self.block_second && second {
            self.entered.cancel();
            self.gate.notified().await;
        }
        Ok((
            Arc::new(TestRuntime {
                fail_setup: self.fail_setup && second,
                fail_cleanup: self.fail_cleanup && second,
                cancel_on_exec: None,
                deleted: Default::default(),
                record: Some((reference.into(), self.events.clone())),
                closed_signal: second.then(|| self.closed_second.clone()),
            }),
            RuntimeIdentity::default(),
        ))
    }
}

#[tokio::test]
async fn activation_cleans_every_choice_before_atomic_publication() {
    for failure in 0..3 {
        let (root, _, previous) = fixture();
        let directory = root.path().join("releases");
        publish(&directory, &previous).unwrap();
        let (_candidate_root, _, candidate) = fixture_with(
            "provider='agentenv'\ntemplate='test-template'\ntemplates=['other','third']",
            "",
        );
        let mut launcher = FixtureLauncher::new();
        launcher.fail_setup = failure == 1;
        launcher.fail_cleanup = failure == 2;
        let launcher = Arc::new(launcher);
        let result = activate_profile(
            candidate.profile,
            BTreeMap::new(),
            directory.clone(),
            CancellationToken::new(),
            launcher.clone(),
        )
        .await;
        let events = launcher.events.lock().unwrap();
        if failure == 0 {
            let release = result.unwrap();
            assert_eq!(release.variants.len(), 2);
            assert_eq!(active(&directory).unwrap()["test"], release.id);
            assert_eq!(
                *events,
                [
                    "open:test-template",
                    "close:test-template",
                    "open:other",
                    "close:other",
                    "open:third",
                    "close:third"
                ]
            );
        } else {
            assert!(result.is_err());
            assert_eq!(active(&directory).unwrap()["test"], previous.id);
            assert_eq!(load_all(&directory).unwrap().len(), 1);
            assert_eq!(
                *events,
                [
                    "open:test-template",
                    "close:test-template",
                    "open:other",
                    "close:other"
                ]
            );
        }
    }
}

#[tokio::test]
async fn dropped_activation_waiter_finishes_inflight_creation_cleanup_and_stops_next_choice() {
    let (root, _, previous) = fixture();
    let directory = root.path().join("releases");
    publish(&directory, &previous).unwrap();
    let (_candidate_root, _, candidate) = fixture_with(
        "provider='agentenv'\ntemplate='test-template'\ntemplates=['other','third']",
        "",
    );
    let mut launcher = FixtureLauncher::new();
    launcher.block_second = true;
    let launcher = Arc::new(launcher);
    let caller = tokio::spawn(activate_profile(
        candidate.profile,
        BTreeMap::new(),
        directory.clone(),
        CancellationToken::new(),
        launcher.clone(),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        launcher.entered.cancelled(),
    )
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    launcher.gate.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        launcher.closed_second.cancelled(),
    )
    .await
    .unwrap();
    assert_eq!(active(&directory).unwrap()["test"], previous.id);
    assert_eq!(
        *launcher.events.lock().unwrap(),
        [
            "open:test-template",
            "close:test-template",
            "open:other",
            "close:other"
        ]
    );
}
