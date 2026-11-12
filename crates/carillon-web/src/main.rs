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

use std::{
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Command as Process, ExitCode},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{
    bundled::Bundled,
    config::{LoadedConfig, ProviderRole, token_matches},
    home,
};
use carillon_interface::instance;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{browser::Key, serve::serve};

/// The `program` of a registry entry this program serves.
const PROGRAM: &str = Bundled::Web.program();

#[derive(Debug, Parser)]
#[command(
    name = "carillon-web",
    version,
    about = "Carillon web chat interface provider",
    after_help = "Run as `carillon web …`. `carillon web open` signs this machine's browser \
                  in and opens the chat."
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
    /// Sign a browser of this machine in and open the chat in it. The link
    /// works once, within two minutes.
    Open {
        /// Print the link instead of opening it.
        #[arg(long)]
        print: bool,
        /// Registry ID of the web instance; inferred when only one is registered.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Sign every browser out and void every file link.
    SignOut {
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
            println!(
                "The web chat is set up at http://{address}. While Carillon runs, `carillon web \
                 open` signs this machine's browser in and opens it."
            );
            Ok(())
        }
        Command::Open { print, provider } => {
            let loaded = installation()?;
            let link = link(&loaded, &instance(&loaded, PROGRAM, provider)?)?;
            // The code travels in the fragment, which no server or log receives.
            if !print
                && Process::new("xdg-open")
                    .arg(&link)
                    .status()
                    .is_ok_and(|status| status.success())
            {
                println!("Opened Carillon's web chat in your browser.");
            } else {
                println!("{link}");
            }
            Ok(())
        }
        Command::SignOut { provider } => {
            let loaded = installation()?;
            let provider = instance(&loaded, PROGRAM, provider)?;
            let key_file = browser::key_file(&loaded.value.data_dir, &provider);
            // A hand-written entry has no directory before its first start.
            if !key_file.parent().is_some_and(Path::is_dir) {
                println!("Carillon's web chat has not started yet, so no browser is signed in.");
                return Ok(());
            }
            Key::replace(&key_file).with_context(|| format!("replace {}", key_file.display()))?;
            println!("Every browser is signed out of Carillon's web chat.");
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

/// The link that signs a browser in, made from the browser key alone: no
/// request carries a credential to whatever listens at the provider's
/// address. The provider is only asked whether it is there.
fn link(loaded: &LoadedConfig, provider: &str) -> Result<String> {
    let address = loaded
        .provider(provider, ProviderRole::Interface)?
        .listen_address()?;
    if TcpStream::connect_timeout(&address, Duration::from_secs(2)).is_err() {
        bail!(
            "Carillon's web chat is not running at http://{address}. Start Carillon with \
             `carillon start`."
        );
    }
    let key_file = browser::key_file(&loaded.value.data_dir, provider);
    let code = browser::new_code(&key_file)
        .with_context(|| format!("read or create {}", key_file.display()))?;
    Ok(format!("http://{address}/#sign-in={code}"))
}
