//! Installation configuration, the programs a release bundles, credentials,
//! location, maintenance lock, the way across a format change, durable
//! filesystem utilities, in-place edits of its TOML files, the loopback HTTP
//! client and setup's terminal questions. Library construction does not
//! discover or modify an installation.

pub mod bundled;
pub mod config;
pub mod crossing;
pub mod edit;
pub mod filesystem;
pub mod home;
pub mod loopback;
pub mod maintenance;
pub mod prompt;
