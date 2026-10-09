//! Private JSONL app-server transport. Stdout is reserved for protocol frames.
use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use aporto_core::{CoreLimits, CoreService, executor::ProductionExecutor};
use aporto_protocol::{INVALID_REQUEST, MAX_FRAME_BYTES, RpcError, RpcRequest, RpcResponse};
use clap::Parser;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Aporto Core JSON-RPC app server over private stdio")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    releases_dir: Option<PathBuf>,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long, default_value_t = 4)]
    max_concurrent: usize,
    #[arg(long, default_value_t = 64)]
    max_queued: usize,
    #[arg(long, default_value_t = 30_000)]
    shutdown_grace_ms: u64,
}

async fn frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if frame.is_empty() { None } else { Some(frame) });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let length = newline.map_or(available.len(), |position| position + 1);
        ensure!(
            frame.len() + length <= MAX_FRAME_BYTES,
            "Core request exceeds protocol frame limit"
        );
        frame.extend_from_slice(&available[..length]);
        reader.consume(length);
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

async fn send<W: AsyncWrite + Unpin>(writer: &mut W, response: RpcResponse) -> Result<()> {
    let mut bytes = serde_json::to_vec(&response)?;
    ensure!(
        bytes.len() < MAX_FRAME_BYTES,
        "Core response exceeds protocol frame limit"
    );
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn serve(service: &CoreService, stop: CancellationToken) -> Result<()> {
    let failure = service.failure_token();
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();
    loop {
        let bytes = tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = failure.cancelled() => anyhow::bail!("Core persistence failed"),
            result = frame(&mut reader) => result?,
        };
        let Some(bytes) = bytes else {
            return Ok(());
        };
        let request = serde_json::from_slice::<RpcRequest>(&bytes);
        let shutdown_request = request
            .as_ref()
            .is_ok_and(|request| request.method == "shutdown");
        let response = match request {
            Ok(request) => tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                _ = failure.cancelled() => anyhow::bail!("Core persistence failed"),
                response = service.handle(request) => response,
            },
            Err(_) => RpcResponse::failure(
                None,
                RpcError::new(INVALID_REQUEST, "invalid JSON-RPC request"),
            ),
        };
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = failure.cancelled() => anyhow::bail!("Core persistence failed"),
            result = send(&mut writer, response) => result?,
        }
        if shutdown_request && service.is_shutting_down() {
            return Ok(());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let executor = Arc::new(
        ProductionExecutor::from_config_file_and_releases(
            &args.config,
            &args
                .releases_dir
                .unwrap_or_else(|| aporto::release::default_directory(&args.config)),
        )
        .context("load Core agent configuration")?,
    );
    let limits = CoreLimits {
        max_concurrent: args.max_concurrent,
        max_queued: args.max_queued,
        shutdown_grace: Duration::from_millis(args.shutdown_grace_ms),
        ..CoreLimits::default()
    };
    let service = CoreService::open(&args.state_dir, executor, limits).await?;
    let stop = CancellationToken::new();
    let signal_stop = stop.clone();
    let signal = tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signal_stop.cancel();
    });
    let outcome = serve(&service, stop).await;
    signal.abort();
    let cleanup = service.shutdown().await;
    cleanup.context("graceful Core shutdown failed")?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn jsonl_framing_preserves_multiple_requests_and_final_unterminated_frame() {
        let mut reader = BufReader::new(&b"{\"id\":1}\n{\"id\":2}"[..]);
        assert_eq!(frame(&mut reader).await.unwrap().unwrap(), b"{\"id\":1}\n");
        assert_eq!(frame(&mut reader).await.unwrap().unwrap(), b"{\"id\":2}");
        assert!(frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_unterminated_request_is_rejected_with_bounded_reads() {
        let input = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut reader = BufReader::new(input.as_slice());
        assert!(frame(&mut reader).await.is_err());
    }
}
