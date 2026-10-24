//! The `carillon` operator command: the core service, installation actions,
//! the authenticated diagnostics client for a running core, and forwarding
//! of provider commands (`carillon discord …`) to the provider's own binary.

use std::{
    ffi::OsString,
    os::unix::process::{CommandExt as _, ExitStatusExt as _},
    path::{Path, PathBuf},
    process::Command as Process,
    time::Duration,
};

use anyhow::{Context as _, Result};
use carillon_installation::{
    config::{LoadedConfig, RELEASE_VERSION},
    home,
    maintenance::MaintenanceLock,
    prompt::Terminal,
};
use carillon_protocol::model::{TurnInspection, TurnSummary};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

use carillon_installation::maintenance::ServiceLock;

use super::{
    admin::{Runtime, backup, initialize as init, restore, set_provider},
    drain::{Pace, drain, stop},
    http::{Applied, RegistryStatus, check, serve},
    operations::{
        Action, Activity, Applying, HeldUnits, Services, apply_registry, logs, operator_get,
        operator_url, print_health, registry_status, status, wait_until_healthy,
    },
    setup::{Options as SetupOptions, setup},
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
    /// Seconds to wait for every enabled provider to answer core, and,
    /// before running services stop, for core to drain.
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    timeout: u64,
}

impl Wait {
    const fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout)
    }
}

/// Whether running services stop only once core has drained.
#[derive(Debug, clap::Args)]
struct Drained {
    /// Stop without waiting for core to drain. A message a runtime is still
    /// running is recorded as interrupted and not replayed, and native work
    /// a runtime started may go on.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new installation.
    Init {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long, default_value_t = 43_800)]
        base_port: u16,
        /// The runtime the agent runs on; only it is registered.
        #[arg(long, value_enum, default_value_t = Runtime::Pi)]
        runtime: Runtime,
    },
    /// Set up an installation at the terminal: the runtimes and what runs
    /// Carillon's assistant, then Discord. A rerun keeps what is already set
    /// up when asked to.
    Setup {
        /// The agent's folder with Pi (default: your home). With T3, the
        /// chosen project's folder.
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value_t = 43_800)]
        base_port: u16,
        /// A runtime to use, instead of asking; repeat it for both.
        #[arg(long, value_enum)]
        runtime: Vec<Runtime>,
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
    /// Restart the installation's user services and wait until they are
    /// healthy. Running services stop once core has drained: no runtime
    /// turn left running, every answer delivered, and every runtime ready
    /// to restart. Otherwise nothing is stopped.
    Restart {
        #[command(flatten)]
        wait: Wait,
        #[command(flatten)]
        drained: Drained,
    },
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
    /// Running services stop once the release is checked and core has
    /// drained, as for `restart`.
    Update {
        archive: PathBuf,
        #[command(flatten)]
        wait: Wait,
        #[command(flatten)]
        drained: Drained,
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
            runtime,
        } => {
            let path = init(&home()?, &workspace, base_port, runtime)?;
            println!("{}", path.display());
        }
        Command::Setup {
            workspace,
            base_port,
            runtime,
        } => setup(
            &home()?,
            &SetupOptions {
                workspace,
                base_port,
                runtimes: runtime,
            },
            &mut Terminal,
        )?,
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
            control(&home, Action::Start).await?;
            report_running(&load(&home)?, &wait).await?;
        }
        Command::Stop => {
            let home = home()?;
            control(&home, Action::Stop).await?;
            println!("Carillon at {} stopped", home.display());
        }
        Command::Restart { wait, drained } => {
            let home = home()?;
            let config = load(&home)?;
            restart(&home, &config, wait.timeout(), drained.force).await?;
            report_running(&config, &wait).await?;
        }
        Command::Logs {
            service,
            lines,
            follow,
        } => logs(&home()?, service.as_deref(), lines, follow)?,
        Command::Update {
            archive,
            wait,
            drained,
        } => update(&home()?, &archive, wait.timeout(), drained.force).await?,
        Command::Provider { id, action } => provider(&home()?, &id, action).await?,
        Command::Registry { action } => registry(&load(&home()?)?, action).await?,
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
            println!("{id} {changed} in core's serving registry.");
            println!(
                "This changes admission and workers, not provider programs. If an enabled \
                 runtime's program is not running, `carillon start` starts it and stops nothing."
            );
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
async fn control(home: &Path, action: Action) -> Result<()> {
    let _lock = MaintenanceLock::acquire(home)?;
    let units = HeldUnits::acquire()?;
    Services::registered(home, &units)?.control(action).await
}

/// Stops running services once core has drained within `timeout`, or
/// forced, then starts them, under the same locks as [`control`].
async fn restart(home: &Path, config: &LoadedConfig, timeout: Duration, force: bool) -> Result<()> {
    let _lock = MaintenanceLock::acquire(home)?;
    let units = HeldUnits::acquire()?;
    let services = Services::registered(home, &units)?;
    if services.activity()? == Activity::Running {
        let hold = drain(config, timeout, force, Pace::OPERATOR)
            .await
            .context("Carillon was not restarted; its services keep running")?;
        stop(&services, hold).await?;
    }
    services.control(Action::Start).await
}

async fn report_running(config: &LoadedConfig, wait: &Wait) -> Result<()> {
    let health = wait_until_healthy(config, wait.timeout())
        .await
        .context("inspect `carillon logs` and `carillon status`")?;
    println!("Carillon at {} is running:", config.root.display());
    print_health(&health);
    Ok(())
}

/// Provider commands that change installation files without a lock of their
/// own: core runs them under the installation's maintenance lock.
const LOCKED: [(&str, &str); 1] = [("runtime-t3", "configure")];

/// Replaces this process with the `carillon-<name>` binary and the remaining
/// arguments, so signals and the exit status are the provider's own. Only
/// names that look like provider ids are forwarded. The provider receives the
/// resolved installation as `CARILLON_HOME`. A [`LOCKED`] command instead
/// runs as a child, and its exit status becomes this process's.
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
    if LOCKED.iter().any(|&(provider, command)| {
        provider == name && rest.first().is_some_and(|first| first == command)
    }) {
        let status = run_locked(&binary, rest, home)?;
        std::process::exit(
            status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .unwrap_or(1),
        );
    }
    let error = Process::new(&binary)
        .args(rest)
        .env("CARILLON_HOME", home)
        .exec();
    Err(error).with_context(|| format!("run {}", binary.display()))
}

/// Runs `binary` with `arguments` while holding the installation's
/// maintenance lock, or under the caller's when it holds it already, and
/// tells the child so. Its standard input stays this process's.
fn run_locked(
    binary: &Path,
    arguments: &[OsString],
    home: &Path,
) -> Result<std::process::ExitStatus> {
    let _maintenance = MaintenanceLock::acquire_unless_held(home).context("nothing was changed")?;
    Process::new(binary)
        .args(arguments)
        .env("CARILLON_HOME", home)
        .env(MaintenanceLock::HELD_FOR, home)
        .status()
        .with_context(|| format!("run {}", binary.display()))
}

/// The bundle's own providers sit beside this binary and take precedence; a
/// provider installed independently is found on `PATH`, which replacing the
/// bundle never touches.
pub(super) fn provider_binary(name: &str) -> Result<PathBuf> {
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

fn print_runtimes(applied: &Applied) {
    for runtime in &applied.runtimes {
        print_runtime(runtime);
    }
    print_paused(applied.paused);
}

fn print_paused(paused: bool) {
    if paused {
        println!("  Maintenance holds the workers paused; they return when it ends.");
    }
}

fn print_runtime(runtime: &super::http::ServingRuntime) {
    let workers = runtime.workers;
    let settling = workers.messages.settling + workers.observations.settling;
    let state = match (runtime.enabled, runtime.durable) {
        (false, _) => "disabled".to_owned(),
        (true, true) => "enabled, holding and ordering its own turns".to_owned(),
        (true, false) => format!(
            "enabled, {} conversation(s) at once",
            runtime.parallel_turns
        ),
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
    print_paused(status.paused);
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
    fn a_locked_provider_command_runs_only_under_the_maintenance_lock() -> Result<()> {
        use carillon_installation::maintenance::MaintenanceLock;

        let directory = tempfile::tempdir()?;
        let home = directory.path();
        let program = home.join("carillon-runtime-t3");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             read -r token\n\
             if flock -n \"$CARILLON_HOME/.maintenance.lock\" true; then lock=free; else lock=held; fi\n\
             echo \"$lock $CARILLON_MAINTENANCE_LOCK $* $token\" >\"$CARILLON_HOME/observed\"\n",
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let arguments = ["configure".into(), "--url".into(), "x".into()];

        let held = MaintenanceLock::acquire(home)?;
        let refusal = super::run_locked(&program, &arguments, home)
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            refusal.contains("another Carillon maintenance operation"),
            "{refusal}"
        );
        assert!(
            !home.join("observed").exists(),
            "it ran during other maintenance"
        );
        drop(held);

        // Standard input reaches it; the token is never an argument.
        let input = home.join("token");
        std::fs::write(&input, "t3-token\n")?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "application::cli::tests::run_locked_child",
                "--ignored",
            ])
            .env("RUN_LOCKED_PROGRAM", &program)
            .env("RUN_LOCKED_HOME", home)
            .stdin(std::fs::File::open(&input)?)
            .stdout(std::process::Stdio::null())
            .status()?;
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(home.join("observed"))?,
            format!("held {} configure --url x t3-token\n", home.display())
        );
        assert!(MaintenanceLock::try_acquire(home)?.is_some());
        Ok(())
    }

    /// [`super::run_locked`] in a process of its own, whose standard input
    /// the test above gives.
    #[test]
    #[ignore = "run by a_locked_provider_command_runs_only_under_the_maintenance_lock"]
    fn run_locked_child() -> Result<()> {
        let program = std::env::var_os("RUN_LOCKED_PROGRAM").context("no program")?;
        let home = std::env::var_os("RUN_LOCKED_HOME").context("no installation")?;
        let arguments = ["configure".into(), "--url".into(), "x".into()];
        let status = super::run_locked(
            std::path::Path::new(&program),
            &arguments,
            std::path::Path::new(&home),
        )?;
        anyhow::ensure!(status.success(), "{status}");
        Ok(())
    }

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
        crate::assert_refused(super::locate_provider("review", &own, &path), "not on PATH")?;
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
