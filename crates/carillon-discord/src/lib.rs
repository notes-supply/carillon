//! Discord components. Importing this library never connects a gateway or starts a server.
mod activity;
pub mod application;
mod attachments;
mod cli;
mod client;
pub mod config;
pub mod conversations;
mod delivery;
mod error;
mod gateway;
mod inbound;
mod module;
mod operations;
mod setup;
#[cfg(test)]
use client::CoreClient;
pub use error::InterfaceError;
