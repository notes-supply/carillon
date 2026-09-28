//! Discord components. Importing this library never connects a gateway or starts a server.
mod activity;
pub mod application;
mod cli;
mod client;
mod commands;
pub mod config;
pub mod conversations;
mod delivery;
mod error;
mod inbound;
mod operations;
pub use client::CoreClient;
pub use error::InterfaceError;
