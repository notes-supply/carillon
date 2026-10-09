//! Installation configuration, credentials, location, maintenance lock,
//! durable filesystem utilities and the loopback HTTP client. Library
//! construction does not discover or modify an installation.

pub mod config;
pub mod filesystem;
pub mod home;
pub mod loopback;
pub mod maintenance;
