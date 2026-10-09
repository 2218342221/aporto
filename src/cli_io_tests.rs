use super::*;
use anyhow::anyhow;
use aporto::types::{CommandResult, RuntimeProcess};
use async_trait::async_trait;
use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Debug, Default)]
struct FakeRuntime {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    commands: Mutex<Vec<String>>,
    closes: AtomicUsize,
    pauses: AtomicUsize,
    pause_fails: bool,
}

#[async_trait]
impl Runtime for FakeRuntime {
    fn id(&self) -> &str {
        "cli-fixture-sandbox"
    }

    async fn exec(&self, command: &str, _: ExecOptions) -> Result<CommandResult> {
        self.commands.lock().unwrap().push(command.to_owned());
        Ok(CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        })
    }

    async fn start_process(&self, _: &[String], _: ExecOptions) -> Result<Box<dyn RuntimeProcess>> {
        bail!("unexpected process start in CLI I/O test")
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .with_context(|| format!("fixture file missing: {path}"))
    }

    async fn write_file(&self, path: &str, bytes: &[u8]) -> Result<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), bytes.to_vec());
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl ManagedRuntime for FakeRuntime {
    async fn pause(&self) -> Result<()> {
        self.pauses.fetch_add(1, Ordering::SeqCst);
        if self.pause_fails {
            bail!("injected pause failure");
        }
        Ok(())
    }
}

struct CreationDrop(Arc<AtomicBool>);

impl Drop for CreationDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancellation_during_creation_waits_for_the_id_then_deletes_once() {
    let runtime = Arc::new(FakeRuntime::default());
    let cancel = CancellationToken::new();
    let creation_dropped = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let creating_runtime = runtime.clone();
    let creating_cancel = cancel.clone();
    let dropped = creation_dropped.clone();
    let mut pending = tokio::spawn(async move {
        await_creation(
            async move {
                let _drop = CreationDrop(dropped);
                started_tx.send(()).unwrap();
                release_rx.await.context("creation release was dropped")?;
                Ok(creating_runtime)
            },
            &creating_cancel,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut pending)
            .await
            .is_err(),
        "cancellation must not discard an outstanding create response"
    );
    assert!(!creation_dropped.load(Ordering::SeqCst));
    assert_eq!(runtime.closes.load(Ordering::SeqCst), 0);

    release_tx.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(error.to_string().contains(runtime.id()));
    assert!(creation_dropped.load(Ordering::SeqCst));
    assert_eq!(runtime.closes.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.pauses.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_before_creation_does_not_dispatch_a_request() {
    let runtime = Arc::new(FakeRuntime::default());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let dispatched = AtomicBool::new(false);
    let error = await_creation(
        async {
            dispatched.store(true, Ordering::SeqCst);
            Ok(runtime.clone())
        },
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(!dispatched.load(Ordering::SeqCst));
    assert_eq!(runtime.closes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_export_retains_the_sandbox_even_when_pause_fails() {
    for pause_fails in [false, true] {
        let runtime = FakeRuntime {
            pause_fails,
            ..Default::default()
        };
        let error = finish_run::<()>(
            &runtime,
            Err(anyhow!("export failed: injected disk full")),
            true,
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(runtime.id()));
        assert!(message.contains("retained"));
        assert!(message.contains("injected disk full"));
        assert_eq!(runtime.pauses.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.closes.load(Ordering::SeqCst), 0);
        if pause_fails {
            assert!(message.contains("injected pause failure"));
        } else {
            assert!(message.contains("paused"));
        }
    }
}

#[tokio::test]
async fn successful_finish_deletes_the_sandbox_and_returns_the_answer() {
    let runtime = FakeRuntime::default();
    let answer = finish_run(&runtime, Ok("finished answer"), false)
        .await
        .unwrap();
    assert_eq!(answer, "finished answer");
    assert_eq!(runtime.closes.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.pauses.load(Ordering::SeqCst), 0);
}

fn directory_names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[tokio::test]
async fn partial_export_write_preserves_the_previous_file_and_removes_temporary_data() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("result.patch");
    std::fs::write(&destination, b"previous complete result").unwrap();
    let error = atomic_write_with(&destination, |mut file| async move {
        file.write_all(b"incomplete replacement").await?;
        file.sync_all().await?;
        bail!("injected write failure")
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("injected write failure"));
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"previous complete result"
    );
    assert_eq!(
        directory_names(root.path()),
        [std::ffi::OsString::from("result.patch")]
    );
}

#[tokio::test]
async fn successful_export_replaces_the_existing_file_without_leaving_temporary_data() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("result.patch");
    std::fs::write(&destination, b"old result").unwrap();
    let runtime = FakeRuntime::default();
    runtime.files.lock().unwrap().insert(
        "/workspace/result.patch".into(),
        b"new complete result".to_vec(),
    );
    let exports = BTreeMap::from([(
        "result.patch".to_owned(),
        destination.to_str().unwrap().to_owned(),
    )]);
    validate_exports(&exports).unwrap();
    export_workspace(&runtime, &exports, "/workspace")
        .await
        .unwrap();
    assert_eq!(std::fs::read(&destination).unwrap(), b"new complete result");
    assert_eq!(
        directory_names(root.path()),
        [std::ffi::OsString::from("result.patch")]
    );
}

#[tokio::test]
async fn workspace_upload_uses_captured_bytes_and_excludes_credential_entries() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir(root.path().join("empty")).unwrap();
    let source = root.path().join("src/main.txt");
    std::fs::write(&source, b"captured contents").unwrap();
    std::fs::write(root.path().join("first.txt"), b"first sibling").unwrap();
    for name in [".env", ".env.production", "client.pem", "private.key"] {
        std::fs::write(root.path().join(name), b"must not upload").unwrap();
        if name == ".env" {
            std::fs::write(root.path().join("middle.txt"), b"middle sibling").unwrap();
        }
    }
    for name in [".git", ".ssh", "node_modules", "target"] {
        let excluded = root.path().join(name);
        std::fs::create_dir(&excluded).unwrap();
        std::fs::write(excluded.join("hidden.txt"), b"must not upload").unwrap();
    }
    std::fs::write(root.path().join("last.txt"), b"last sibling").unwrap();

    let snapshot = prepare_workspace(root.path()).unwrap();
    std::fs::write(&source, b"changed after capture").unwrap();
    std::fs::write(root.path().join("late.txt"), b"not in snapshot").unwrap();
    let runtime = FakeRuntime::default();
    upload_workspace(&runtime, snapshot, "/workspace")
        .await
        .unwrap();
    assert_eq!(
        *runtime.files.lock().unwrap(),
        BTreeMap::from([
            (
                "/workspace/src/main.txt".to_owned(),
                b"captured contents".to_vec()
            ),
            ("/workspace/first.txt".to_owned(), b"first sibling".to_vec()),
            (
                "/workspace/middle.txt".to_owned(),
                b"middle sibling".to_vec()
            ),
            ("/workspace/last.txt".to_owned(), b"last sibling".to_vec()),
        ])
    );
    let commands = runtime.commands.lock().unwrap();
    assert!(
        commands
            .iter()
            .any(|command| command.contains("/workspace/empty"))
    );
    for excluded in [".git", ".ssh", "node_modules", "target"] {
        assert!(commands.iter().all(|command| !command.contains(excluded)));
    }
}

#[test]
fn workspace_scan_limit_counts_ignored_entries_and_empty_directories() {
    let root = tempfile::tempdir().unwrap();
    // Neither half reaches the scan limit alone, and there are no included files.
    // A scan that fails to account for either ignored files or empty directories
    // would incorrectly accept this workspace.
    for index in 0..20_001 {
        if index % 2 == 0 {
            std::fs::write(root.path().join(format!("ignored-{index}.key")), []).unwrap();
        } else {
            std::fs::create_dir(root.path().join(format!("empty-{index}"))).unwrap();
        }
    }
    let error = prepare_workspace(root.path())
        .err()
        .expect("too many scanned entries must fail before upload");
    assert!(error.to_string().contains("scan exceeds 20000 entries"));
}

#[tokio::test]
async fn upload_and_export_share_the_selected_container_directory() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("receipt.txt"), b"custom-workdir-receipt").unwrap();
    let snapshot = prepare_workspace(source.path()).unwrap();
    let runtime = FakeRuntime::default();
    upload_workspace(&runtime, snapshot, "/project/custom")
        .await
        .unwrap();
    assert!(
        runtime
            .files
            .lock()
            .unwrap()
            .contains_key("/project/custom/receipt.txt")
    );
    assert!(
        !runtime
            .files
            .lock()
            .unwrap()
            .contains_key("/workspace/receipt.txt")
    );
    let destination = tempfile::tempdir().unwrap();
    let target = destination.path().join("receipt.txt");
    export_workspace(
        &runtime,
        &BTreeMap::from([("receipt.txt".into(), target.display().to_string())]),
        "/project/custom",
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(target).unwrap(), b"custom-workdir-receipt");
}

#[test]
fn workspace_rejects_depth_65_instead_of_silently_omitting_it() {
    let root = tempfile::tempdir().unwrap();
    let mut nested = root.path().to_owned();
    for _ in 0..64 {
        nested.push("nested");
        std::fs::create_dir(&nested).unwrap();
    }
    prepare_workspace(root.path()).unwrap();
    nested.push("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("must-not-be-omitted.txt"), b"contents").unwrap();
    let error = prepare_workspace(root.path())
        .err()
        .expect("excessive nesting must fail before upload");
    assert!(error.to_string().contains("nesting exceeds 64 levels"));
}
