//! Single-run CLI lifecycle and bounded local artifact I/O.
use anyhow::{Context, Result, bail, ensure};
use aporto::types::{ExecOptions, ManagedRuntime, Runtime};
use std::{
    collections::BTreeMap,
    future::Future,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const MAX_SCAN_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 64;
const MAX_FILES: usize = 10_000;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

pub fn guest_relative(path: &Path) -> Result<String> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "expected a nonempty workspace-relative path"
    );
    Ok(format!(
        "/workspace/{}",
        path.to_str().context("path must be UTF-8")?
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct SnapshotEntry {
    guest: String,
    file: Option<(Vec<u8>, bool)>,
}

pub struct WorkspaceSnapshot {
    entries: Vec<SnapshotEntry>,
    files: usize,
    bytes: usize,
}

/// Capture all included bytes before creating or mutating a remote sandbox.
/// As with Agentfile builds, the input directory must not be concurrently replaced.
pub fn prepare_workspace(root: &Path) -> Result<WorkspaceSnapshot> {
    ensure!(root.is_dir(), "workspace must be a directory");
    let mut snapshot = WorkspaceSnapshot {
        entries: Vec::new(),
        files: 0,
        bytes: 0,
    };
    let mut scanned = 0usize;
    let mut walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
        // Avoid WalkDir buffering an entire ancestor directory when its open
        // descriptor limit is reached; traversal depth is bounded below.
        .max_open(MAX_DEPTH + 2)
        .max_depth(MAX_DEPTH + 1)
        .into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry?;
        scanned += 1;
        ensure!(
            scanned <= MAX_SCAN_ENTRIES,
            "workspace scan exceeds {MAX_SCAN_ENTRIES} entries"
        );
        let name = entry.file_name().to_string_lossy();
        if entry.depth() != 0
            && (matches!(
                name.as_ref(),
                ".git" | ".ssh" | "node_modules" | "target" | ".env"
            ) || name.starts_with(".env.")
                || name.ends_with(".pem")
                || name.ends_with(".key"))
        {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        ensure!(
            entry.depth() <= MAX_DEPTH,
            "workspace nesting exceeds {MAX_DEPTH} levels"
        );
        ensure!(
            !entry.file_type().is_symlink(),
            "workspace symlink not supported: {}",
            entry.path().display()
        );
        if entry.depth() == 0 {
            continue;
        }
        let guest = guest_relative(entry.path().strip_prefix(root)?)?;
        let file = if entry.file_type().is_dir() {
            None
        } else {
            ensure!(
                entry.file_type().is_file(),
                "workspace contains a special file"
            );
            let metadata = entry.metadata()?;
            snapshot.files += 1;
            ensure!(
                metadata.len() <= MAX_FILE_BYTES && snapshot.files <= MAX_FILES,
                "workspace snapshot exceeds limit (16 MiB/file, 64 MiB total, 10000 files)"
            );
            let mut bytes = Vec::new();
            std::fs::File::open(entry.path())?
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            snapshot.bytes += bytes.len();
            ensure!(
                bytes.len() as u64 <= MAX_FILE_BYTES && snapshot.bytes <= MAX_TOTAL_BYTES,
                "workspace snapshot exceeds byte limit"
            );
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            Some((bytes, executable))
        };
        snapshot.entries.push(SnapshotEntry { guest, file });
    }
    Ok(snapshot)
}

pub async fn upload_workspace(
    runtime: &dyn Runtime,
    snapshot: WorkspaceSnapshot,
    workdir: &str,
) -> Result<()> {
    let workdir = aporto::workspace::validate_workdir(workdir)?;
    for mut entry in snapshot.entries {
        let relative = entry
            .guest
            .strip_prefix("/workspace/")
            .context("invalid captured workspace path")?;
        entry.guest = aporto::workspace::resolve_file(&workdir, relative)?;
        if let Some((bytes, executable)) = entry.file {
            runtime.write_file(&entry.guest, &bytes).await?;
            let mode = if executable { "0755" } else { "0644" };
            let result = runtime
                .exec(
                    &format!("chmod {mode} -- {}", shell_quote(&entry.guest)),
                    ExecOptions::default(),
                )
                .await?;
            ensure!(
                result.exit_code == 0,
                "cannot set workspace file permissions"
            );
        } else {
            let result = runtime
                .exec(
                    &format!("mkdir -p -- {}", shell_quote(&entry.guest)),
                    ExecOptions::default(),
                )
                .await?;
            ensure!(result.exit_code == 0, "cannot create workspace directory");
        }
    }
    eprintln!(
        "uploaded workspace snapshot: {} files, {} bytes",
        snapshot.files, snapshot.bytes
    );
    Ok(())
}

pub fn validate_exports(exports: &BTreeMap<String, String>) -> Result<()> {
    for (guest, local) in exports {
        guest_relative(Path::new(guest))?;
        let local = Path::new(local);
        ensure!(
            local.file_name().is_some(),
            "export destination needs a file name"
        );
        ensure!(
            output_parent(local).is_dir(),
            "export parent directory does not exist: {}",
            local.display()
        );
        ensure!(
            !local.is_dir(),
            "export destination is a directory: {}",
            local.display()
        );
    }
    Ok(())
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

async fn atomic_write_with<F, Fut>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(tokio::fs::File) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let temporary: PathBuf =
        output_parent(path).join(format!(".aporto-export-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(&temporary).await?;
    let result = async {
        write(file).await?;
        tokio::fs::rename(&temporary, path).await?;
        #[cfg(unix)]
        tokio::fs::File::open(output_parent(path))
            .await?
            .sync_all()
            .await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

pub async fn export_workspace(
    runtime: &dyn Runtime,
    exports: &BTreeMap<String, String>,
    workdir: &str,
) -> Result<()> {
    let workdir = aporto::workspace::validate_workdir(workdir)?;
    for (guest, local) in exports {
        guest_relative(Path::new(guest))?;
        let bytes = runtime
            .read_file(&aporto::workspace::resolve_file(&workdir, guest)?)
            .await
            .with_context(|| format!("read export {guest}"))?;
        atomic_write_with(Path::new(local), |mut file| async move {
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            Ok(())
        })
        .await
        .with_context(|| format!("export {guest} to {local}"))?;
    }
    Ok(())
}

/// Register the OS handler before returning; spawning an unpolled ctrl_c future
/// alone would leave the creation request exposed to the default SIGINT action.
pub fn cancel_on_interrupt(cancel: CancellationToken) -> Result<SignalTask> {
    #[cfg(unix)]
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    #[cfg(windows)]
    let mut signal = tokio::signal::windows::ctrl_c()?;
    Ok(SignalTask(tokio::spawn(async move {
        signal.recv().await;
        cancel.cancel();
    })))
}

pub struct SignalTask(tokio::task::JoinHandle<()>);
impl Drop for SignalTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Do not cancel/drop creation: wait for its bounded SDK result so a newly
/// allocated sandbox ID remains available for explicit deletion.
pub async fn await_creation<R: Runtime + ?Sized>(
    creation: impl Future<Output = Result<Arc<R>>>,
    cancel: &CancellationToken,
) -> Result<Arc<R>> {
    ensure!(!cancel.is_cancelled(), "sandbox creation cancelled");
    let runtime = creation.await?;
    if cancel.is_cancelled() {
        runtime.close().await.with_context(|| {
            format!(
                "cancelled during creation; sandbox cleanup failed for {}",
                runtime.id()
            )
        })?;
        bail!(
            "sandbox creation cancelled; sandbox {} deleted",
            runtime.id()
        );
    }
    Ok(runtime)
}

pub async fn finish_run<T>(
    runtime: &dyn ManagedRuntime,
    outcome: Result<T>,
    preserve: bool,
) -> Result<T> {
    if preserve {
        let pause = runtime.pause().await;
        let status = match pause {
            Ok(()) => "paused".to_owned(),
            Err(error) => {
                format!("pause failed: {error}; inspect the runtime and stop remaining work")
            }
        };
        let reason = match outcome {
            Ok(_) => "export was not confirmed".into(),
            Err(error) => format!("{error:#}"),
        };
        bail!(
            "export failed; workspace {} retained ({status}); recover it using its runtime to retry export: {reason}",
            runtime.id()
        );
    }
    runtime.close().await.with_context(|| {
        format!(
            "sandbox cleanup failed for {}; run result: {}",
            runtime.id(),
            if outcome.is_ok() {
                "completed"
            } else {
                "failed"
            }
        )
    })?;
    outcome
}

#[cfg(test)]
#[path = "cli_io_tests.rs"]
mod tests;
