#![forbid(unsafe_code)]
//! Durable product state and composition for Carillon.

mod admin;
mod cli;
mod core;
mod db;
mod resources;

/// Runs the Carillon operator command.
///
/// # Errors
///
/// Returns an error when configuration, durable storage, a component, or an
/// operator-requested action fails.
pub async fn run() -> anyhow::Result<()> {
    cli::run().await
}
