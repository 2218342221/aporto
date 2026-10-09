use aporto_core_client::{ClientOptions, CoreClient, CoreProcessConfig};
use aporto_protocol::{InitializeParams, InitializeResult, PROTOCOL_VERSION};
use aporto_server::{ServerConfig, router};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    version,
    about = "Serve the Aporto HTTP API through an isolated Core process"
)]
struct Cli {
    #[arg(long, default_value = "aporto-core")]
    core_bin: PathBuf,
    #[arg(long)]
    core_config: PathBuf,
    /// Defaults to releases/ beside the deployment TOML.
    #[arg(long)]
    releases_dir: Option<PathBuf>,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    #[arg(long, default_value = "APORTO_SERVER_TOKEN")]
    token_env: String,
    #[arg(long = "allow-origin")]
    allow_origin: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let token = std::env::var(&cli.token_env).map_err(|_| {
        format!(
            "missing operator token environment variable {}",
            cli.token_env
        )
    })?;
    let core = CoreClient::spawn(
        CoreProcessConfig {
            binary: cli.core_bin,
            config: cli.core_config,
            state_dir: cli.state_dir,
            releases_dir: cli.releases_dir,
        },
        ClientOptions::default(),
    )
    .await?;
    let result = async {
        let initialized: InitializeResult = core
            .call(
                "initialize",
                InitializeParams {
                    protocol_version: PROTOCOL_VERSION.into(),
                    client_name: "aporto-server".into(),
                },
            )
            .await?;
        if initialized.protocol_version != PROTOCOL_VERSION {
            return Err("Core protocol version mismatch".into());
        }
        let stop = CancellationToken::new();
        let app = router(
            core.clone(),
            ServerConfig::new(token, cli.allow_origin),
            stop.clone(),
        )
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let listener = tokio::net::TcpListener::bind(cli.listen).await?;
        // Register Unix signal handlers before announcing readiness to a supervisor.
        let signal = wait_for_shutdown();
        tokio::pin!(signal);
        eprintln!("Aporto Server listening on {}", listener.local_addr()?);
        let server_stop = stop.clone();
        let server = async {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_stop.cancelled_owned())
                .await
        };
        tokio::pin!(server);
        let core_exited = tokio::select! {
            // A supervisor can signal the whole process group at once. An explicit
            // shutdown already received takes precedence over a simultaneous exit.
            biased;
            () = &mut signal => false,
            () = core.wait_closed() => true,
            result = &mut server => {
                stop.cancel();
                result?;
                return Ok(());
            },
        };
        stop.cancel();
        match tokio::time::timeout(std::time::Duration::from_secs(10), &mut server).await {
            Ok(result) => result?,
            Err(_) => eprintln!("HTTP drain timed out; closing remaining connections"),
        }
        if core_exited {
            return Err("Core process exited unexpectedly".into());
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    let cleanup = core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

fn wait_for_shutdown() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    let signals = (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
    );
    async move {
        #[cfg(unix)]
        if let (Ok(mut term), Ok(mut interrupt)) = signals {
            tokio::select! {
                _ = term.recv() => {},
                _ = interrupt.recv() => {},
            }
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}
