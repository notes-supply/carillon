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
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    channels::SignInCode,
    serve::{ServeOptions, serve},
};

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
            let options = ServeOptions {
                provider_id: provider,
                token_file,
                site: match site {
                    Some(site) => site,
                    None => release_site()?,
                },
            };
            serve(LoadedConfig::load(&config)?, options).await
        }
        Command::Open { print, provider } => {
            let loaded = installation()?;
            open(&loaded, &instance(&loaded, provider)?, print).await
        }
        Command::SignOut { provider } => {
            let loaded = installation()?;
            sign_out(&loaded, &instance(&loaded, provider)?).await
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

/// The web instance an operator action names, or the only one registered.
fn instance(config: &LoadedConfig, explicit: Option<String>) -> Result<String> {
    let mut instances = config.value.providers.iter();
    if let Some(explicit) = explicit {
        return match instances.find(|entry| entry.id == explicit) {
            Some(entry) if entry.program == PROGRAM => Ok(explicit),
            Some(entry) => bail!(
                "{explicit} is not a web instance: its program is \"{}\"",
                entry.program
            ),
            None => bail!("provider {explicit} is not configured"),
        };
    }
    let instances: Vec<&str> = instances
        .filter(|entry| entry.program == PROGRAM)
        .map(|entry| entry.id.as_str())
        .collect();
    match instances.as_slice() {
        [only] => Ok((*only).to_owned()),
        [] => bail!("carillon.toml registers no provider with program = \"{PROGRAM}\""),
        several => bail!(
            "several web instances are registered; choose one with --provider from: {}",
            several.join(", ")
        ),
    }
}

/// Posts to the running provider's operator API with the operator's
/// credential.
async fn operator(
    loaded: &LoadedConfig,
    url: &str,
    action: &str,
) -> Result<reqwest::Result<reqwest::Response>> {
    Ok(loopback::client()
        .timeout(Duration::from_secs(10))
        .build()?
        .post(format!("{url}/v1/{action}"))
        .bearer_auth(loaded.credential("operator")?)
        .send()
        .await)
}

async fn accepted(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("the web provider refused the request ({status}): {body}");
    }
    Ok(response)
}

async fn open(loaded: &LoadedConfig, provider: &str, print: bool) -> Result<()> {
    let url = &loaded.provider(provider, ProviderRole::Interface)?.url;
    let response = operator(loaded, url, "sign-in")
        .await?
        .with_context(|| format!("the web provider does not answer at {url}"))?;
    let SignInCode { code } = accepted(response).await?.json().await?;
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
    let url = &loaded.provider(provider, ProviderRole::Interface)?.url;
    match operator(loaded, url, "sign-out").await? {
        Ok(response) => {
            accepted(response).await?;
            println!("Every browser is signed out of Carillon's web chat.");
        }
        // Nothing serves the key, so replacing its file is the whole of it.
        Err(error) if error.is_connect() => {
            let key_file = browser::key_file(&loaded.value.data_dir, provider);
            if key_file.exists() {
                browser::replace_key(&key_file)
                    .with_context(|| format!("replace {}", key_file.display()))?;
            }
            println!(
                "The web provider is not running. No browser is signed in when it starts again."
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("the web provider did not answer at {url}"));
        }
    }
    Ok(())
}
