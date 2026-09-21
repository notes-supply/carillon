//! The Carillon Discord interface provider.

mod cli;
mod commands;
mod config;
mod provider;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
