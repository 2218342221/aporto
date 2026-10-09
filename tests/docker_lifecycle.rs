#![cfg(unix)]
//! Deterministic lifecycle failures through a stateful fake Docker CLI; no daemon.
use aporto::{
    runtime::{DockerConfig, DockerRuntime},
    types::Runtime,
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tempfile::TempDir;

struct Fake {
    directory: TempDir,
    config: DockerConfig,
}
impl Fake {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut config = DockerConfig::new("fixture:latest", "fixture-owner");
        // Keep executable code immutable while parallel tests spawn processes;
        // each fixture writes state only beside its unique fake socket path.
        config.binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/docker_cli.py");
        config.host = format!("unix://{}", directory.path().join("docker.sock").display());
        Self { directory, config }
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.directory.path().join(name)
    }
    fn flag(&self, name: &str) {
        std::fs::write(self.path(name), "1").unwrap();
    }
    fn clear(&self, name: &str) {
        std::fs::remove_file(self.path(name)).unwrap();
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.path("state.json")).unwrap()).unwrap()
    }
    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    async fn open(&self) -> std::sync::Arc<DockerRuntime> {
        DockerRuntime::open(self.config.clone(), None)
            .await
            .unwrap()
    }
}
async fn marker(path: &Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fake CLI did not reach controlled lifecycle point");
}
async fn drain_tasks() {
    // Any old Drop retry schedules its CLI immediately. Give the process-backed
    // fixture time to record it; failures are controlled by files, never timing.
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test]
async fn failed_explicit_cleanup_does_not_stop_a_later_reopened_runtime_on_drop() {
    for action in ["stop", "rm"] {
        let fake = Fake::new();
        let old = fake.open().await;
        let id = old.id().to_owned();
        let failure = format!("fail-{action}");
        fake.flag(&failure);
        let failed = if action == "stop" {
            old.pause().await
        } else {
            old.close().await
        };
        assert!(failed.is_err());
        assert_eq!(fake.state()["running"], true);
        fake.clear(&failure);
        // Reconnect reconciles the same owned instance before starting a new turn.
        let next = DockerRuntime::open(fake.config.clone(), Some(&id))
            .await
            .unwrap();
        let before_drop = fake.calls();
        assert_eq!(fake.state()["running"], true);
        drop(old);
        drain_tasks().await;
        assert_eq!(
            fake.calls(),
            before_drop,
            "old failed {action} scheduled detached cleanup after reconnect"
        );
        assert_eq!(fake.state()["running"], true);
        next.close().await.unwrap();
    }
}

#[tokio::test]
async fn failed_cleanup_is_still_explicitly_retryable_and_success_is_idempotent() {
    let fake = Fake::new();
    let runtime = fake.open().await;
    fake.flag("fail-stop");
    assert!(runtime.pause().await.is_err());
    fake.clear("fail-stop");
    runtime.pause().await.unwrap();
    let calls = fake.calls();
    runtime.pause().await.unwrap();
    assert_eq!(fake.calls(), calls);
    fake.flag("fail-rm");
    assert!(runtime.close().await.is_err());
    fake.clear("fail-rm");
    runtime.close().await.unwrap();
    runtime.close().await.unwrap();
    assert_eq!(fake.state()["exists"], false);
    let before_drop = fake.calls();
    drop(runtime);
    drain_tasks().await;
    assert_eq!(fake.calls(), before_drop);
}

#[tokio::test]
async fn startup_contract_failure_has_one_synchronous_cleanup_owner() {
    for create in [true, false] {
        let fake = Fake::new();
        let id = if create {
            None
        } else {
            let runtime = fake.open().await;
            let id = runtime.id().to_owned();
            runtime.pause().await.unwrap();
            Some(id)
        };
        let before = fake.calls().len();
        fake.flag("fail-contract");
        assert!(
            DockerRuntime::open(fake.config.clone(), id.as_deref())
                .await
                .is_err()
        );
        drain_tasks().await;
        let calls = fake.calls();
        let cleanup: Vec<_> = calls[before..]
            .iter()
            .filter(|call| {
                call[0] == "container"
                    && ["inspect", "stop", "rm"]
                        .iter()
                        .any(|kind| call[1] == *kind)
            })
            .cloned()
            .collect();
        assert_eq!(
            cleanup,
            if create {
                vec![json!(["container", "inspect"]), json!(["container", "rm"])]
            } else {
                vec![
                    json!(["container", "inspect"]),
                    json!(["container", "inspect"]),
                    json!(["container", "stop"]),
                ]
            }
        );
        assert_eq!(fake.state()["running"], false);
        fake.clear("fail-contract");
        if let Some(id) = id {
            let next = DockerRuntime::open(fake.config.clone(), Some(&id))
                .await
                .unwrap();
            assert_eq!(fake.state()["running"], true);
            next.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn dropping_a_lifecycle_waiter_does_not_abandon_its_supervised_cleanup() {
    let fake = Fake::new();
    let runtime = fake.open().await;
    fake.flag("hold-stop");
    let running = runtime.clone();
    let waiter = tokio::spawn(async move { running.pause().await });
    marker(&fake.path("stop.entered")).await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    fake.clear("hold-stop");
    // An explicit second pause waits on the SAME Inner lifecycle lock, proving
    // that the detached supervisor completed before the second operation returns.
    runtime.pause().await.unwrap();
    assert_eq!(fake.state()["running"], false);
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|call| **call == json!(["container", "stop"]))
            .count(),
        1
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn abandoned_startup_waiter_removes_its_undelivered_container() {
    let fake = Fake::new();
    fake.flag("hold-ready");
    let config = fake.config.clone();
    let waiter = tokio::spawn(async move { DockerRuntime::open(config, None).await });
    marker(&fake.path("ready.entered")).await;
    waiter.abort();
    assert!(waiter.await.is_err());
    fake.clear("hold-ready");
    marker(&fake.path("rm.entered")).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while fake.state()["exists"] == true {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("startup supervisor abandoned its container");
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|call| **call == json!(["container", "rm"]))
            .count(),
        1
    );
}
