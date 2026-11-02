//! Installation and service assembly; importing the core does not start it.
mod admin;
mod cli;
mod drain;
mod http;
mod operations;
mod settings;
mod setup;
mod update;

/// Run the installed application's command-line entry point.
///
/// # Errors
/// Returns configuration, installation, transport or service errors.
pub async fn run() -> anyhow::Result<()> {
    cli::run().await
}
