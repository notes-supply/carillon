//! The programs a release bundles, and what an installation's registry says
//! of the provider each serves. Initialization, setup, updates and the
//! providers' own commands all read this one list.

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::config::{ProviderRole, RELEASE_VERSION};

/// A program this release bundles. `carillon-<program>` serves every
/// registry entry that names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bundled {
    Discord,
    Pi,
    T3,
}

impl Bundled {
    pub const ALL: [Self; 3] = [Self::Discord, Self::Pi, Self::T3];

    /// The registry's `program`, and the `carillon <program>` command.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Pi => "runtime-pi",
            Self::T3 => "runtime-t3",
        }
    }

    /// The ID of the entry setup registers for it.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Discord => "discord-private",
            Self::Pi => "runtime-pi",
            Self::T3 => "runtime-t3",
        }
    }

    #[must_use]
    pub const fn role(self) -> ProviderRole {
        match self {
            Self::Discord => ProviderRole::Interface,
            Self::Pi | Self::T3 => ProviderRole::Runtime,
        }
    }

    /// Its port's distance above core's, the same in every installation, so
    /// adding a program never moves another.
    #[must_use]
    pub const fn port_offset(self) -> u16 {
        match self {
            Self::Discord => 1,
            Self::Pi => 2,
            Self::T3 => 3,
        }
    }

    /// The version of the program this release bundles: the workspace's for
    /// a Rust program, its package's for a runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when a runtime's package does not state its version.
    pub fn version(self) -> Result<String> {
        #[derive(Deserialize)]
        struct Package {
            version: String,
        }
        let package = match self {
            Self::Discord => return Ok(RELEASE_VERSION.to_owned()),
            Self::Pi => include_str!("../../../packages/runtime-pi/package.json"),
            Self::T3 => include_str!("../../../packages/runtime-t3/package.json"),
        };
        Ok(serde_json::from_str::<Package>(package)?.version)
    }

    /// What an operator calls it: its program, without a runtime's prefix.
    #[must_use]
    pub fn name(self) -> &'static str {
        let program = self.program();
        program.strip_prefix("runtime-").unwrap_or(program)
    }

    /// The bundled program a registry entry's `program` names, if any.
    #[must_use]
    pub fn of(program: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|bundled| bundled.program() == program)
    }

    /// The bundled programs of `role`, in the order setup offers them.
    pub fn of_role(role: ProviderRole) -> impl Iterator<Item = Self> + Clone {
        Self::ALL
            .into_iter()
            .filter(move |bundled| bundled.role() == role)
    }

    /// The bundled program of `role` an operator calls `name`.
    ///
    /// # Errors
    ///
    /// Returns the names `role` offers when `name` is none of them.
    pub fn named(role: ProviderRole, name: &str) -> Result<Self> {
        Self::of_role(role)
            .find(|bundled| bundled.name() == name)
            .with_context(|| {
                let names: Vec<&str> = Self::of_role(role).map(Self::name).collect();
                format!("expected one of: {}", names.join(", "))
            })
    }
}
