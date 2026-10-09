//! The `carillon` operator command: the core service, installation actions,
//! the authenticated diagnostics client for a running core, and forwarding
//! of provider commands (`carillon discord …`) to the provider's own binary.

use std::{
    ffi::OsString,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::Command as Process,
    time::Duration,
};

use anyhow::{Context as _, Result};
use carillon_installation::{
    config::{LoadedConfig, RELEASE_VERSION},
    home,
    maintenance::MaintenanceLock,
};
use carillon_protocol::model::{TurnInspection, TurnSummary};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

use carillon_installation::maintenance::ServiceLock;

use super::{
    admin::{backup, enable_demo_resources, initialize as init, restore, set_provider},
    http::{Applied, RegistryStatus, check, serve},
    operations::{
        Action, Applying, HeldUnits, Services, apply_registry, logs, operator_get, operator_url,
        print_health, registry_status, status, wait_until_healthy,
    },
    update::update,
};

#[derive(Debug, Parser)]
#[command(
    name = "carillon",
    version,
    about = "Self-hosted agent service",
    after_help = "Provider commands: `carillon <provider> …` runs that provider's own program, \
                  for example `carillon discord --help` for Discord settings and slash-command \
                  modules."
)]
struct Cli {
    /// Installation to act on. Defaults to `CARILLON_HOME`, then the
    /// installation containing this program, then `~/.carillon`.
    #[arg(long, global = true, value_name = "PATH")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ProviderAction {
    Enable,
    Disable,
}

#[derive(Debug, Subcommand)]
enum RegistryAction {
    /// Have the running core serve `carillon.toml`.
    Apply,
}

#[derive(Debug, clap::Args)]
struct Wait {
    /// Seconds to wait for every enabled provider to answer core.
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    timeout: u64,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new installation.
    Init {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long, default_value_t = 43_800)]
        base_port: u16,
    },
    /// Run the core service with an explicit registry.
    Core {
        #[arg(long)]
        config: PathBuf,
    },
    /// Report core and provider health from the running core.
    Status,
    /// Check, without changing anything, that this program accepts the
    /// installation's registry and core database.
    Check,
    /// Start the installation's user services and wait until they are healthy.
    Start(Wait),
    /// Stop the installation's user services.
    Stop,
    /// Restart the installation's user services and wait until they are healthy.
    Restart(Wait),
    /// Show service logs.
    Logs {
        /// Only this log, such as `core` or `discord`.
        service: Option<String>,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: u32,
        #[arg(short, long)]
        follow: bool,
    },
    /// Replace the application from a local release archive, keeping
    /// configuration and state, and restore the previous running state.
    Update {
        archive: PathBuf,
        #[command(flatten)]
        wait: Wait,
    },
    /// Enable or disable a provider in the registry. A running core applies
    /// a runtime's change at once; an interface's needs a restart.
    Provider {
        /// The provider's registry ID.
        id: String,
        action: ProviderAction,
    },
    /// Show the registry core serves, with its runtimes' workers, and how
    /// `carillon.toml` differs from it; or have core serve the file.
    #[command(after_help = "\
Core serves a changed runtime `enabled` or `parallel_turns` without a
restart: a disabled runtime admits no new work and starts no queued turn,
while turns it is running settle and their answers are delivered; fewer
parallel turns let running turns finish first. Applying neither starts nor
stops a runtime's program. A file that also changes anything else, such as
addresses, credentials, the agent or the set of providers, is refused whole
and applies when Carillon restarts.")]
    Registry {
        #[command(subcommand)]
        action: Option<RegistryAction>,
    },
    /// Add the example Pi extension and Agent Skill to the agent resources.
    EnableDemoResources,
    /// List recent turns from the running core.
    Recent {
        /// Only the turn for this external event.
        #[arg(long)]
        event_id: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show one turn's state, output or failure explanation.
    Inspect { turn_id: String },
    /// Archive the stopped installation, including credentials and state.
    Backup {
        /// Archive to create, outside the installation.
        #[arg(long)]
        output: PathBuf,
    },
    /// Publish an installation from a backup archive.
    Restore {
        /// Archive created by `carillon backup`.
        #[arg(long)]
        input: PathBuf,
        /// Installation directory to create; must be absent or empty.
        #[arg(long)]
        target: PathBuf,
    },
    /// `carillon <provider> …` runs `carillon-<provider>` from beside this
    /// binary or from `PATH` with the resolved `CARILLON_HOME`, so provider
    /// commands share one entry point without core knowing them.
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
    let Cli { home, command } = Cli::parse();
    // The service and restore name their files explicitly; everything else
    // acts on the resolved installation.
    let home = || home::resolve(home.as_deref());
    let load = |home: &Path| LoadedConfig::load(&home.join("carillon.toml"));
    match command {
        Command::Core { config } => {
            let loaded = LoadedConfig::load(&config)?;
            let _lock = ServiceLock::acquire(&loaded.value.data_dir)?;
            serve(config.canonicalize()?, loaded).await?;
        }
        Command::Restore { input, target } => {
            println!("{}", restore(&input, &target)?.display());
        }
        Command::Init {
            workspace,
            base_port,
        } => {
            let path = init(&home()?, &workspace, base_port)?;
            println!("{}", path.display());
        }
        Command::External(arguments) => forward(&arguments, &home()?)?,
        Command::Status => print_pretty(&status(&load(&home()?)?).await?)?,
        Command::Check => {
            let home = home()?;
            check(&load(&home)?).with_context(|| {
                format!(
                    "Carillon {} cannot use the installation at {}",
                    RELEASE_VERSION,
                    home.display()
                )
            })?;
            println!(
                "registry and core database at {} are compatible with Carillon {}",
                home.display(),
                RELEASE_VERSION
            );
        }
        Command::Start(wait) => {
            let home = home()?;
            control(&home, Action::Start)?;
            report_running(&load(&home)?, &wait).await?;
        }
        Command::Stop => {
            let home = home()?;
            control(&home, Action::Stop)?;
            println!("Carillon at {} stopped", home.display());
        }
        Command::Restart(wait) => {
            let home = home()?;
            control(&home, Action::Restart)?;
            report_running(&load(&home)?, &wait).await?;
        }
        Command::Logs {
            service,
            lines,
            follow,
        } => logs(&home()?, service.as_deref(), lines, follow)?,
        Command::Update { archive, wait } => {
            update(&home()?, &archive, Duration::from_secs(wait.timeout)).await?;
        }
        Command::Provider { id, action } => provider(&home()?, &id, action).await?,
        Command::Registry { action } => registry(&load(&home()?)?, action).await?,
        Command::EnableDemoResources => {
            enable_demo_resources(&load(&home()?)?)?;
            println!("Pi-native extension and Agent Skill enabled");
        }
        Command::Recent { event_id, limit } => {
            print_recent(&load(&home()?)?, event_id.as_deref(), limit).await?;
        }
        Command::Inspect { turn_id } => print_inspection(&load(&home()?)?, &turn_id).await?,
        Command::Backup { output } => {
            let home = home()?;
            let _lock = MaintenanceLock::acquire(&home)?;
            backup(&load(&home)?, &output)?;
            println!("{}", output.display());
        }
    }
    Ok(())
}

/// Enables or disables a provider in `carillon.toml`, then has a running
/// core serve the change when it applies live.
async fn provider(home: &Path, id: &str, action: ProviderAction) -> Result<()> {
    let enabled = action == ProviderAction::Enable;
    set_provider(&home.join("carillon.toml"), id, enabled)?;
    let changed = if enabled { "enabled" } else { "disabled" };
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    match apply_registry(&config).await? {
        Applying::Applied(applied) => {
            println!("{id} {changed}; core serves it now.");
            print_runtimes(&applied);
        }
        Applying::Refused(refusal) => println!(
            "{id} {changed} in carillon.toml; core keeps serving the registry it has: \
             {refusal}. `carillon restart` applies it."
        ),
        Applying::Unreachable => println!(
            "{id} {changed} in carillon.toml. Core does not answer at {}, so whether it serves \
             the change is not confirmed: a stopped core serves it when it starts, and \
             `carillon registry` shows what a running one serves.",
            config.value.core.listen
        ),
    }
    Ok(())
}

/// Shows the registry core serves, or has core serve `carillon.toml`.
async fn registry(config: &LoadedConfig, action: Option<RegistryAction>) -> Result<()> {
    let Some(RegistryAction::Apply) = action else {
        print_registry(&registry_status(config).await?);
        return Ok(());
    };
    match apply_registry(config).await? {
        Applying::Applied(applied) => {
            if applied.changed.is_empty() {
                println!("core already serves carillon.toml.");
            } else {
                println!("core now serves: {}.", applied.changed.join(", "));
            }
            print_runtimes(&applied);
            Ok(())
        }
        Applying::Refused(refusal) => anyhow::bail!("{refusal}"),
        Applying::Unreachable => anyhow::bail!(
            "core does not answer at {}, so it did not confirm serving carillon.toml; a stopped \
             core serves it when it starts",
            config.value.core.listen
        ),
    }
}

/// Act on this installation's units, serialized with this installation's
/// maintenance and with every installation's use of the owner's units.
fn control(home: &Path, action: Action) -> Result<()> {
    let _lock = MaintenanceLock::acquire(home)?;
    let units = HeldUnits::acquire()?;
    Services::registered(home, &units)?.control(action)
}

async fn report_running(config: &LoadedConfig, wait: &Wait) -> Result<()> {
    let health = wait_until_healthy(config, Duration::from_secs(wait.timeout))
        .await
        .context("inspect `carillon logs` and `carillon status`")?;
    println!("Carillon at {} is running:", config.root.display());
    print_health(&health);
    Ok(())
}

/// Replaces this process with the `carillon-<name>` binary and the remaining
/// arguments, so signals and the exit status are the provider's own. Only
/// names that look like provider ids are forwarded. The provider receives the
/// resolved installation as `CARILLON_HOME`.
fn forward(arguments: &[OsString], home: &Path) -> Result<()> {
    let (name, rest) = arguments
        .split_first()
        .context("provider command name is missing")?;
    let name = name
        .to_str()
        .context("provider command name is not UTF-8")?;
    carillon_installation::config::validate_provider_identity(name)
        .with_context(|| format!("unknown command {name}"))?;
    let binary = provider_binary(name)?;
    let error = Process::new(&binary)
        .args(rest)
        .env("CARILLON_HOME", home)
        .exec();
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

async fn print_recent(config: &LoadedConfig, event_id: Option<&str>, limit: u32) -> Result<()> {
    let mut url = operator_url(config, "/v1/turns")?;
    url.query_pairs_mut()
        .append_pair("limit", &limit.to_string());
    if let Some(event_id) = event_id {
        url.query_pairs_mut().append_pair("event_id", event_id);
    }
    let turns: Vec<TurnSummary> = operator_get(config, url).await?;
    print_pretty(&turns)
}

async fn print_inspection(config: &LoadedConfig, turn_id: &str) -> Result<()> {
    let inspection: TurnInspection = operator_get(
        config,
        operator_url(config, &format!("/v1/turns/{turn_id}"))?,
    )
    .await?;
    print_pretty(&inspection)
}

/// Each runtime as core serves it.
fn print_runtimes(applied: &Applied) {
    for runtime in &applied.runtimes {
        print_runtime(runtime);
    }
}

fn print_runtime(runtime: &super::http::ServingRuntime) {
    let workers = runtime.workers;
    let settling = workers.messages.settling + workers.observations.settling;
    let state = if runtime.enabled {
        format!(
            "enabled, {} conversation(s) at once",
            runtime.parallel_turns
        )
    } else {
        "disabled".to_owned()
    };
    println!(
        "  {}: {state}; {} message and {} observation worker(s){}",
        runtime.id,
        workers.messages.claiming,
        workers.observations.claiming,
        if settling > 0 {
            format!(", {settling} more settling the turn they hold")
        } else {
            String::new()
        }
    );
}

fn print_registry(status: &RegistryStatus) {
    use super::http::FileState;
    println!("core serves:");
    for runtime in &status.runtimes {
        print_runtime(runtime);
    }
    match &status.file {
        FileState::Read(difference)
            if difference.live.is_empty() && difference.restart.is_empty() =>
        {
            println!("carillon.toml is what core serves.");
        }
        FileState::Read(difference) => {
            if !difference.live.is_empty() {
                println!(
                    "carillon.toml changes what core can apply now ({}); `carillon registry apply` serves it.",
                    difference.live.join(", ")
                );
            }
            if !difference.restart.is_empty() {
                println!(
                    "carillon.toml changes what only a restart applies ({}); `carillon restart` serves it.",
                    difference.restart.join(", ")
                );
            }
        }
        FileState::Invalid { error } => println!("carillon.toml is not a valid registry: {error}"),
    }
}

fn print_pretty<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use anyhow::{Context, Result};

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
}
