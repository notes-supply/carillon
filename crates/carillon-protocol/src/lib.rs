#![forbid(unsafe_code)]
//! Installation configuration, credentials, and wire types shared by Carillon
//! core and its providers. Platform-specific configuration stays with the
//! provider that owns it.

pub mod config;
pub mod filesystem;
pub mod model;
