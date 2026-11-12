//! Discord components. Importing this library never connects a gateway or starts a server.
mod activity;
pub mod application;
mod attachments;
mod channels;
mod cli;
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
pub use carillon_interface::InterfaceError;

#[cfg(test)]
fn test_client(base_url: String) -> anyhow::Result<carillon_interface::CoreClient> {
    carillon_interface::CoreClient::new(base_url, "discord".to_owned(), "token".to_owned())
}
