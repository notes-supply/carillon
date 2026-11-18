//! Transport-neutral wire vocabulary shared by core and providers.
//! No installation discovery, credentials, filesystem operations or startup.
#![feature(must_not_suspend)]

pub mod conversations;
mod fields;
pub mod model;
pub mod observation;
pub mod settings;
pub mod turns;
