//! Discord components. Importing this library never connects a gateway or starts a server.
pub mod application;
mod cli;
mod client;
mod commands;
pub mod config;
pub mod conversations;
mod delivery;
mod error;
pub use client::CoreClient;
pub use error::InterfaceError;
