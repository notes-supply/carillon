//! The `carillon` operator command: the core service, installation actions,
//! the authenticated diagnostics client for a running core, and forwarding
//! of provider commands (`carillon discord …`) to the provider's own binary.

use std::{
    ffi::OsString,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::Command as Process,
};

use anyhow::{Context as _, Result};
use carillon_installation::config::LoadedConfig;
use carillon_protocol::model::{StatusReport, TurnInspection, TurnSummary};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::Url;
use serde::{Serialize, de::DeserializeOwned};
use tracing_subscriber::EnvFilter;

use crate::{
    admin::{
        ServiceLock, backup, enable_demo_resources, initialize as init, restore, set_provider,
    },
    core::serve,
};

#[derive(Debug, Parser)]
#[command(name = "carillon", version, about = "Self-hosted agent service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ProviderAction {
    Enable,
    Disable,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long, default_value_t = 43_800)]
        base_port: u16,
    },
    Core {
        #[arg(long)]
        config: PathBuf,
    },

    Provider {
        #[arg(long)]
        config: PathBuf,
        id: String,
        action: ProviderAction,
    },
    EnableDemoResources {
        #[arg(long)]
        config: PathBuf,
    },
    Status {
        #[arg(long)]
        config: PathBuf,
    },
    Recent {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        event_id: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    Inspect {
        #[arg(long)]
        config: PathBuf,
        turn_id: String,
    },
    Backup {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    Restore {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        target: PathBuf,
    },
    /// `carillon <provider> …` runs `carillon-<provider>` from beside this
    /// binary or from `PATH`, so provider commands share one entry point
    /// without core knowing them.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

fn initialize_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("carillon=info")),
        )
        .with_target(false)
        .init();
}

pub(crate) async fn run() -> Result<()> {
    initialize_tracing();
    match Cli::parse().command {
        Command::Init {
            root,
            workspace,
            base_port,
        } => {
            let path = init(&root, &workspace, base_port)?;
            println!("{}", path.display());
        }
        Command::Core { config } => {
            let loaded = LoadedConfig::load(&config)?;
            let _lock = ServiceLock::acquire(&loaded.value.data_dir)?;
            serve(loaded).await?;
        }
        Command::External(arguments) => forward(&arguments)?,

        Command::Provider { config, id, action } => {
            set_provider(&config, &id, action == ProviderAction::Enable)?;
            let outcome = match action {
                ProviderAction::Enable => "enabled",
                ProviderAction::Disable => "disabled",
            };
            println!("{id} {outcome}");
        }
        Command::EnableDemoResources { config } => {
            enable_demo_resources(&LoadedConfig::load(&config)?)?;
            println!("Pi-native extension and Agent Skill enabled");
        }
        Command::Status { config } => print_status(&config).await?,
        Command::Recent {
            config,
            event_id,
            limit,
        } => print_recent(&config, event_id.as_deref(), limit).await?,
        Command::Inspect { config, turn_id } => print_inspection(&config, &turn_id).await?,
        Command::Backup { config, output } => {
            backup(&LoadedConfig::load(&config)?, &output)?;
            println!("{}", output.display());
        }
        Command::Restore { input, target } => {
            println!("{}", restore(&input, &target)?.display());
        }
    }
    Ok(())
}

/// Replaces this process with the `carillon-<name>` binary and the remaining
/// arguments, so signals and the exit status are the provider's own. Only
/// names that look like provider ids are forwarded.
fn forward(arguments: &[OsString]) -> Result<()> {
    let (name, rest) = arguments
        .split_first()
        .context("provider command name is missing")?;
    let name = name
        .to_str()
        .context("provider command name is not UTF-8")?;
    carillon_installation::config::validate_provider_identity(name)
        .with_context(|| format!("unknown command {name}"))?;
    let binary = provider_binary(name)?;
    let error = Process::new(&binary).args(rest).exec();
    Err(error).with_context(|| format!("run {}", binary.display()))
}

/// The bundle's own providers sit beside this binary and take precedence; a
/// provider installed independently is found on `PATH`, which replacing the
/// bundle never touches.
fn provider_binary(name: &str) -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let own = executable
        .parent()
        .context("current executable has no directory")?;
    locate_provider(name, own, &std::env::var_os("PATH").unwrap_or_default())
}

fn locate_provider(name: &str, own: &Path, path: &std::ffi::OsStr) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let file = format!("carillon-{name}");
    let own = own.join(&file);
    if own.is_file() {
        return Ok(own);
    }
    std::env::split_paths(path)
        .map(|directory| directory.join(&file))
        .find(|candidate| {
            candidate.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .with_context(|| {
            format!(
                "unknown command {name}: {} is not installed and {file} is not on PATH",
                own.display()
            )
        })
}

async fn print_status(config: &Path) -> Result<()> {
    let loaded = LoadedConfig::load(config)?;
    let report: StatusReport = operator_get(&loaded, operator_url(&loaded, "/v1/status")?).await?;
    print_pretty(&report)
}

async fn print_recent(config: &Path, event_id: Option<&str>, limit: u32) -> Result<()> {
    let loaded = LoadedConfig::load(config)?;
    let mut url = operator_url(&loaded, "/v1/turns")?;
    url.query_pairs_mut()
        .append_pair("limit", &limit.to_string());
    if let Some(event_id) = event_id {
        url.query_pairs_mut().append_pair("event_id", event_id);
    }
    let turns: Vec<TurnSummary> = operator_get(&loaded, url).await?;
    print_pretty(&turns)
}

async fn print_inspection(config: &Path, turn_id: &str) -> Result<()> {
    let loaded = LoadedConfig::load(config)?;
    let inspection: TurnInspection = operator_get(
        &loaded,
        operator_url(&loaded, &format!("/v1/turns/{turn_id}"))?,
    )
    .await?;
    print_pretty(&inspection)
}

fn operator_url(config: &LoadedConfig, path: &str) -> Result<Url> {
    Ok(Url::parse(&format!(
        "http://{}{path}",
        config.value.core.listen
    ))?)
}

async fn operator_get<T: DeserializeOwned>(config: &LoadedConfig, url: Url) -> Result<T> {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(config.credential("operator")?)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

fn print_pretty<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt as _, time::Duration};

    use anyhow::{Context, Result};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn providers_are_found_beside_the_binary_then_on_path() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let own = directory.path().join("app");
        let elsewhere = directory.path().join("local");
        let unrelated = directory.path().join("unrelated");
        for place in [&own, &elsewhere, &unrelated] {
            std::fs::create_dir(place)?;
        }
        let path = std::env::join_paths([&unrelated, &elsewhere])?;

        let missing = super::locate_provider("review", &own, &path)
            .err()
            .context("found a provider that is not installed")?;
        assert!(missing.to_string().contains("not on PATH"), "{missing}");

        // A file on PATH without the executable bit is not a command.
        std::fs::write(elsewhere.join("carillon-review"), "")?;
        assert!(super::locate_provider("review", &own, &path).is_err());
        std::fs::set_permissions(
            elsewhere.join("carillon-review"),
            std::fs::Permissions::from_mode(0o755),
        )?;
        assert_eq!(
            super::locate_provider("review", &own, &path)?,
            elsewhere.join("carillon-review")
        );

        // The bundle's own provider wins over one of the same name on PATH.
        std::fs::write(own.join("carillon-review"), "")?;
        assert_eq!(
            super::locate_provider("review", &own, &path)?,
            own.join("carillon-review")
        );
        Ok(())
    }

    #[tokio::test]
    async fn operator_requests_time_out_on_stalled_headers_and_bodies() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config_path = crate::admin::initialize(
            &directory.path().join("installation"),
            directory.path(),
            44_000,
        )?;
        let config = super::LoadedConfig::load(&config_path)?;
        for partial_body in [false, true] {
            // Keep slow diagnostic fixtures away from backup's 127.0.0.1 probes.
            let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
            let url = format!("http://{}/v1/status", listener.local_addr()?).parse()?;
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await?);
                }
                if partial_body {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                        .await?;
                }
                // The request deadline must close this connection without a response.
                let mut remaining = Vec::new();
                stream.read_to_end(&mut remaining).await?;
                Ok::<_, std::io::Error>(())
            });
            let result = tokio::time::timeout(
                Duration::from_secs(7),
                super::operator_get::<serde_json::Value>(&config, url),
            )
            .await;
            server.abort();
            let error = result?.err().context("stalled diagnostics succeeded")?;
            assert!(
                error
                    .downcast_ref::<reqwest::Error>()
                    .context("not an HTTP error")?
                    .is_timeout()
            );
        }
        Ok(())
    }
}
