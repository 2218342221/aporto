use super::DockerConfig;
use anyhow::{Context, Result, ensure};
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub(super) struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(super) fn command(config: &DockerConfig) -> Command {
    let mut command = Command::new(&config.binary);
    // Never forward the operator's model/runtime credentials to subprocesses.
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .arg("--host")
        .arg(&config.host)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

pub(super) async fn run(config: &DockerConfig, args: &[&str]) -> Result<Output> {
    let mut child = command(config)
        .args(args)
        .spawn()
        .context("cannot launch local Docker CLI")?;
    let stdout = child.stdout.take().context("Docker stdout unavailable")?;
    let stderr = child.stderr.take().context("Docker stderr unavailable")?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let stdout = bounded(stdout, 1024 * 1024);
        let stderr = bounded(stderr, 64 * 1024);
        let (stdout, stderr, status) = tokio::try_join!(stdout, stderr, async {
            child.wait().await.context("cannot wait for Docker CLI")
        })?;
        Ok::<_, anyhow::Error>(Output {
            success: status.success(),
            stdout,
            stderr,
        })
    })
    .await
    .context("local Docker operation timed out")?
}

pub(super) async fn checked(config: &DockerConfig, args: &[&str]) -> Result<Vec<u8>> {
    let output = run(config, args).await?;
    ensure!(
        output.success,
        "local Docker {} failed",
        args.first().unwrap_or(&"operation")
    );
    Ok(output.stdout)
}

pub(super) async fn bounded(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "Docker output exceeds limit");
    Ok(bytes)
}
