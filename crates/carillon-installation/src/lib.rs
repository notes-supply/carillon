//! Installation configuration, credentials, location, maintenance lock,
//! durable filesystem utilities, the loopback HTTP client and setup's
//! terminal questions. Library construction does not discover or modify an
//! installation.

pub mod config;
pub mod filesystem;
pub mod home;
pub mod loopback;
pub mod maintenance;
pub mod prompt;
