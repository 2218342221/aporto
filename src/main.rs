mod cli_io;

use anyhow::{Context, Result, bail, ensure};
use aporto::{
    broker::create_broker_in_workspace,
    build::{build, read_bundle},
    model::run_agent,
    release,
    runtime::open_backend,
    types::ExecOptions,
};
use clap::{Parser, Subcommand};
use cli_io::{
    await_creation, cancel_on_interrupt, export_workspace, finish_run, prepare_workspace,
    upload_workspace, validate_exports,
};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    version,
    about = "Build, activate, and run agents defined with Agentfile"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct DeploymentOptions {
    /// Explicit deployment TOML; never discovered from the task workspace.
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    agent: String,
    /// Defaults to releases/ beside the deployment file.
    #[arg(long)]
    releases_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Snapshot explicitly declared resources offline. No code or MCP is executed.
    Build {
        #[arg(default_value = ".")]
        context: PathBuf,
        #[arg(short = 'f', long, default_value = "Agentfile")]
        file: PathBuf,
        #[arg(short, long, default_value = "image.agent.json")]
        output: PathBuf,
        #[arg(long = "context", value_name = "NAME=PATH")]
        named_contexts: Vec<String>,
    },
    /// Verify bundle integrity and inspect its behavior and dependencies.
    Inspect { bundle: PathBuf },
    /// Verify the runtime and MCP catalog, then publish an immutable release.
    Activate {
        #[command(flatten)]
        deployment: DeploymentOptions,
    },
    /// Run the active release using a Responses model and its declared runtime.
    Run {
        #[command(flatten)]
        deployment: DeploymentOptions,
        #[arg(long)]
        task: String,
        /// Upload task files into the selected working directory without discovering configuration.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Select an image/template from the activated Agent allowlist.
        #[arg(long)]
        image: Option<String>,
        /// Container-local directory; defaults to the Agent's configured workdir.
        #[arg(long)]
        workdir: Option<String>,
        /// Save a guest file before teardown, e.g. result.patch=./result.patch.
        #[arg(long = "export", value_name = "GUEST_RELATIVE=LOCAL_PATH")]
        exports: Vec<String>,
    },
}

fn pairs(values: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    for value in values {
        let (key, val) = value.split_once('=').context("expected NAME=VALUE")?;
        ensure!(
            !key.is_empty() && !val.is_empty(),
            "empty NAME=VALUE component"
        );
        ensure!(
            pairs.insert(key.to_owned(), val.to_owned()).is_none(),
            "duplicate binding {key}"
        );
    }
    Ok(pairs)
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Build {
            context,
            file,
            output,
            named_contexts,
        } => {
            let contexts = pairs(named_contexts)?
                .into_iter()
                .map(|(key, value)| (key, PathBuf::from(value)))
                .collect();
            let bundle = build(&context, &file, &contexts)?;
            tokio::fs::write(&output, serde_json::to_vec_pretty(&bundle)?).await?;
            println!("{} {}", bundle.digest, output.display());
        }
        Command::Inspect { bundle } => {
            let bundle = read_bundle(&bundle)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"digest":bundle.digest,
                "file_count":bundle.files.len(),"manifest":bundle.manifest}))?
            );
        }
        Command::Activate { deployment } => {
            let directory = deployment
                .releases_dir
                .unwrap_or_else(|| release::default_directory(&deployment.config));
            let cancel = CancellationToken::new();
            let _signal = cancel_on_interrupt(cancel.clone())?;
            let release = release::activate_with_cancel(
                &deployment.config,
                &deployment.agent,
                &directory,
                cancel,
            )
            .await?;
            println!("{}", release.id);
        }
        Command::Run {
            deployment,
            task,
            workspace,
            image,
            workdir,
            exports,
        } => {
            let directory = deployment
                .releases_dir
                .unwrap_or_else(|| release::default_directory(&deployment.config));
            let release = release::load_active(&deployment.config, &deployment.agent, &directory)?;
            let bundle = &release.profile.bundle;
            let image = image
                .as_deref()
                .unwrap_or(bundle.manifest.runtime.reference());
            let workdir = aporto::workspace::validate_workdir(
                workdir
                    .as_deref()
                    .unwrap_or(bundle.manifest.runtime.workdir()),
            )?;
            let catalog = release.catalog_for(image)?;
            let mut model = release.profile.connection.resolve()?;
            let secrets = release.profile.resolve_secrets()?;
            model.redacted_values.extend(secrets.values().cloned());
            let export_paths = pairs(exports)?;
            validate_exports(&export_paths)?;
            let snapshot = workspace.as_deref().map(prepare_workspace).transpose()?;
            let cancel = CancellationToken::new();
            let _signal = cancel_on_interrupt(cancel.clone())?;
            let runtime_config = release.resolve_backend_for(image).await?;
            if let aporto::runtime::BackendConfig::AgentEnv(config) = &runtime_config {
                model.redacted_values.push(config.api_key.clone());
            }
            let runtime = await_creation(open_backend(runtime_config, None), &cancel).await?;
            eprintln!(
                "{} workspace {} / release {}",
                bundle.manifest.runtime.provider(),
                runtime.id(),
                release.id
            );
            let mut preserve_workspace = false;
            let outcome = async {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => bail!("workspace setup cancelled"),
                    result = async {
                        let setup = runtime.exec(&format!("mkdir -p -- {}", aporto::workspace::shell_quote(&workdir)), ExecOptions {cwd:Some("/".into()), ..Default::default()}).await?;
                        ensure!(setup.exit_code == 0, "cannot initialize working directory");
                        if let Some(snapshot) = snapshot { upload_workspace(runtime.as_ref(), snapshot, &workdir).await?; }
                        Ok::<(), anyhow::Error>(())
                    } => result?,
                }
                let mut broker = create_broker_in_workspace(bundle.clone(), runtime.clone(), secrets, &workdir, cancel.clone()).await?;
                if let Err(error) = broker.enforce_catalog(catalog) {
                    broker.close().await.context("close MCP after contract change")?;
                    return Err(error);
                }
                let broker = Arc::new(broker);
                let result = run_agent(bundle, broker.clone(), model, &task, cancel).await;
                let save_result = export_workspace(runtime.as_ref(), &export_paths, &workdir).await;
                preserve_workspace = save_result.is_err();
                let close_result = broker.close().await;
                save_result?;
                close_result?;
                result
            }.await;
            println!(
                "{}",
                finish_run(runtime.as_ref(), outcome, preserve_workspace)
                    .await?
                    .answer
            );
        }
    }
    Ok(())
}
