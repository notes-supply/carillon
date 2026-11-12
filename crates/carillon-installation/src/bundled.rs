//! The programs a release bundles, and what an installation's registry says
//! of the provider each serves.

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::config::{DEFAULT_UPLOAD_MIB, DeliveryMode, ProviderRole, RELEASE_VERSION};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bundled {
    Web,
    Discord,
    Pi,
    T3,
}

impl Bundled {
    pub const ALL: [Self; 4] = [Self::Web, Self::Discord, Self::Pi, Self::T3];

    /// The registry's `program`, and the `carillon <program>` command.
    #[must_use]
    pub const fn program(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Discord => "discord",
            Self::Pi => "runtime-pi",
            Self::T3 => "runtime-t3",
        }
    }

    /// The ID of the entry setup registers for it.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Discord => "discord-private",
            Self::Pi => "runtime-pi",
            Self::T3 => "runtime-t3",
        }
    }

    #[must_use]
    pub const fn role(self) -> ProviderRole {
        match self {
            Self::Web | Self::Discord => ProviderRole::Interface,
            Self::Pi | Self::T3 => ProviderRole::Runtime,
        }
    }

    /// How its entry says an answer reaches people. A web chat's people read
    /// answers from core's record; Discord sends each one itself.
    #[must_use]
    pub const fn delivery(self) -> DeliveryMode {
        match self {
            Self::Web => DeliveryMode::Retained,
            Self::Discord | Self::Pi | Self::T3 => DeliveryMode::External,
        }
    }

    /// Its entry's `max_upload_mib`: Discord's own limit, and far more for
    /// the web chat, whose files a browser fetches from this machine. A
    /// runtime's entry has none.
    #[must_use]
    pub const fn max_upload_mib(self) -> Option<u64> {
        match self {
            Self::Web => Some(1024),
            Self::Discord => Some(DEFAULT_UPLOAD_MIB),
            Self::Pi | Self::T3 => None,
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
            Self::Web => 4,
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
            Self::Web | Self::Discord => return Ok(RELEASE_VERSION.to_owned()),
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

    #[must_use]
    pub fn of(program: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|bundled| bundled.program() == program)
    }

    /// The bundled programs of `role`, in the order setup offers them, the
    /// one it preselects first.
    pub fn of_role(role: ProviderRole) -> impl Iterator<Item = Self> {
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

#[cfg(test)]
mod tests {
    use super::Bundled;
    use crate::config::{DeliveryMode, ProviderRole, RELEASE_VERSION};

    #[test]
    fn the_web_chat_is_an_interface_whose_answers_core_retains() -> anyhow::Result<()> {
        let web = Bundled::named(ProviderRole::Interface, "web")?;
        assert_eq!(web, Bundled::Web);
        assert_eq!(Bundled::of("web"), Some(web));
        assert_eq!((web.id(), web.port_offset()), ("web", 4));
        assert_eq!(web.version()?, RELEASE_VERSION);
        assert_eq!(web.delivery(), DeliveryMode::Retained);
        assert_eq!(web.max_upload_mib(), Some(1024));
        Ok(())
    }

    #[test]
    fn only_the_web_chat_differs_from_an_entry_s_defaults() {
        assert_eq!(Bundled::Discord.delivery(), DeliveryMode::External);
        assert_eq!(Bundled::Discord.max_upload_mib(), Some(20));
        for runtime in Bundled::of_role(ProviderRole::Runtime) {
            assert_eq!(runtime.delivery(), DeliveryMode::External);
            assert_eq!(runtime.max_upload_mib(), None);
        }
    }

    #[test]
    fn every_program_has_its_own_name_and_port() {
        for (index, program) in Bundled::ALL.into_iter().enumerate() {
            for other in &Bundled::ALL[index + 1..] {
                assert_ne!(program.program(), other.program());
                assert_ne!(program.id(), other.id());
                assert_ne!(program.port_offset(), other.port_offset());
            }
        }
    }
}
