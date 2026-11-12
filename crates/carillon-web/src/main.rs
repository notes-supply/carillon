//! `carillon-web`: the web chat's interface provider and its operator
//! commands. Core's `carillon web …` forwards here with the resolved
//! `CARILLON_HOME`; the service itself is started with explicit files.

mod api;
mod browser;
mod channels;
mod files;
#[cfg(test)]
mod fixture;
mod identity;
mod markdown;
mod send;
mod serve;
mod site;

use std::{
    path::{Path, PathBuf},
    process::{Command as Process, ExitCode},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{
    config::{LoadedConfig, ProviderRole},
    home, loopback,
};
use carillon_interface::instance;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{channels::SignInCode, serve::serve};

/// The `program` of a registry entry this program serves.
const PROGRAM: &str = "web";

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
        Command::Open { print, provider } => {
            let loaded = installation()?;
            open(&loaded, &instance(&loaded, PROGRAM, provider)?, print).await
        }
        Command::SignOut { provider } => {
            let loaded = installation()?;
            sign_out(&loaded, &instance(&loaded, PROGRAM, provider)?).await
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

/// Posts to the running provider's operator API with the operator's
/// credential, and answers its URL with what it said.
async fn operator(
    loaded: &LoadedConfig,
    provider: &str,
    action: &str,
) -> Result<(String, reqwest::Response)> {
    let url = &loaded.provider(provider, ProviderRole::Interface)?.url;
    let response = loopback::client()
        .timeout(Duration::from_secs(10))
        .build()?
        .post(format!("{url}/v1/{action}"))
        .bearer_auth(loaded.credential("operator")?)
        .send()
        .await
        .with_context(|| format!("the web provider does not answer at {url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("the web provider refused the request ({status}): {body}");
    }
    Ok((url.clone(), response))
}

async fn open(loaded: &LoadedConfig, provider: &str, print: bool) -> Result<()> {
    let (url, response) = operator(loaded, provider, "sign-in").await?;
    let SignInCode { code } = response.json().await?;
    // The code travels in the fragment, which no server or log receives.
    let link = format!("{url}/#sign-in={code}");
    if !print
        && Process::new("xdg-open")
            .arg(&link)
            .status()
            .is_ok_and(|status| status.success())
    {
        println!("Opened Carillon's web chat at {url} in your browser.");
    } else {
        println!("{link}");
    }
    Ok(())
}

async fn sign_out(loaded: &LoadedConfig, provider: &str) -> Result<()> {
    operator(loaded, provider, "sign-out").await?;
    println!("Every browser is signed out of Carillon's web chat.");
    Ok(())
}
