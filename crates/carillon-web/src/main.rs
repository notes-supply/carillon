//! `carillon-web`: the web chat's interface provider and its operator
//! commands. Core's `carillon web …` forwards here with the resolved
//! `CARILLON_HOME`; the service itself is started with explicit files.

mod api;
mod browser;
mod files;
#[cfg(test)]
mod fixture;
mod identity;
mod markdown;
mod send;
mod serve;
mod site;
mod snapshot;
mod stream;

use std::{
    io::{IsTerminal as _, stdin},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{
    bundled::Bundled,
    config::{LoadedConfig, ProviderRole, token_matches},
    home,
    prompt::{Questions as _, Terminal},
};
use carillon_interface::instance;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::serve::serve;

/// The `program` of a registry entry this program serves.
const PROGRAM: &str = Bundled::Web.program();

#[derive(Debug, Parser)]
#[command(
    name = "carillon-web",
    version,
    about = "Carillon web chat interface provider",
    after_help = "Run as `carillon web …`. `carillon web password` chooses the password the \
                  chat asks for."
)]
struct Cli {
    /// Installation for operator actions. Defaults to `CARILLON_HOME`, then
    /// the installation containing this program, then `~/.carillon`.
    #[arg(long, global = true, value_name = "PATH")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the provider (started by the installed user service).
    Serve {
        #[arg(long)]
        config: PathBuf,
        /// The provider's registry ID.
        #[arg(long)]
        provider: String,
        /// The provider's core credential.
        #[arg(long)]
        token_file: PathBuf,
        /// The directory of the page's files. Defaults to `lib/web` beside
        /// this program's `bin`, where a release has it.
        #[arg(long)]
        site: Option<PathBuf>,
    },
    /// Check the web chat's registry entry and say how to open it (run by
    /// `carillon setup`). It asks nothing.
    Setup {
        /// Registry ID of the web instance; inferred when only one is registered.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Choose the chat's password, which signs every browser out and voids
    /// every file link. It asks twice at a terminal, and otherwise reads
    /// one line from standard input.
    Password {
        /// Registry ID of the web instance; inferred when only one is registered.
        #[arg(long)]
        provider: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("carillon_web=info,carillon_interface=info")),
        )
        .with_target(false)
        .init();
    let Cli { home, command } = Cli::parse();
    let installation = || -> Result<LoadedConfig> {
        LoadedConfig::load(&home::resolve(home.as_deref())?.join("carillon.toml"))
    };
    match command {
        Command::Serve {
            config,
            provider,
            token_file,
            site,
        } => {
            let site = match site {
                Some(site) => site,
                None => release_site()?,
            };
            serve(LoadedConfig::load(&config)?, &provider, &token_file, site).await
        }
        Command::Setup { provider } => {
            let loaded = installation()?;
            let provider = instance(&loaded, PROGRAM, provider)?;
            let entry = loaded.provider(&provider, ProviderRole::Interface)?;
            let address = entry.listen_address()?;
            if !token_matches(&entry.token_hash, &loaded.credential(&provider)?) {
                bail!("the credential of {provider} does not match its entry in carillon.toml");
            }
            serve::retained(entry)?;
            browser::addresses(&loaded, &provider)?;
            println!(
                "The web chat is set up at http://{address}. `carillon web password` chooses or \
                 changes the password it asks for."
            );
            Ok(())
        }
        Command::Password { provider } => {
            let loaded = installation()?;
            let provider = instance(&loaded, PROGRAM, provider)?;
            let entry = loaded.provider(&provider, ProviderRole::Interface)?;
            let address = entry.listen_address()?;
            let password = if stdin().is_terminal() {
                let password =
                    Terminal.secret("The web chat's password (12 characters or more):")?;
                if Terminal.secret("The same password again:")? != password {
                    bail!("the two passwords differ, so the web chat's password is as it was");
                }
                password
            } else {
                let mut line = String::new();
                stdin().read_line(&mut line)?;
                line
            };
            browser::set_password(
                &browser::password_file(&loaded.value.data_dir, &provider),
                &password,
            )?;
            println!(
                "The web chat's password is set. Any browser signed in before is signed out. Sign \
                 in at http://{address}."
            );
            Ok(())
        }
    }
}

/// Where a release keeps the page: `lib/web` in the directory holding this
/// program's `bin`.
fn release_site() -> Result<PathBuf> {
    let executable = std::env::current_exe()?.canonicalize()?;
    executable
        .parent()
        .and_then(Path::parent)
        .map(|release| release.join("lib").join("web"))
        .context("cannot locate this program's release")
}
