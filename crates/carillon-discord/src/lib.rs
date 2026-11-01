//! Discord components. Importing this library never connects a gateway or starts a server.
mod activity;
pub mod application;
mod attachments;
mod channels;
mod cli;
mod client;
pub mod config;
mod controls;
pub mod conversations;
mod delivery;
mod error;
mod gateway;
mod inbound;
mod module;
mod operations;
mod setup;
pub use error::InterfaceError;
