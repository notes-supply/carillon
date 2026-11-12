//! What every interface provider does with core and the installation, and
//! nothing about any platform: joining the installation, core's interface
//! API, staging files for core, and checking the operator's credential.
//!
//! A provider owns its own server, routes and loops. Wire types stay in
//! `carillon-protocol`.
mod client;
mod error;
mod operator;
mod staging;
mod start;
mod submission;

pub use client::{CoreClient, Deliveries, RUNTIME_CALL_TIMEOUT, RUNTIME_LATE};
pub use error::InterfaceError;
pub use operator::{authorize_operator, bearer};
pub use staging::{Batch, Staging};
pub use start::{Started, start};
pub use submission::{settled, submit_event_with_retry};
