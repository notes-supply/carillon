//! The replaceable Discord module: the commands the Discord provider serves,
//! with their definitions, handlers and presentation, and the work cards it
//! shows of jobs. The provider keeps the gateway connection, admission, card
//! lifecycle and every Discord or core call; this module only decides what
//! to show and what to ask for.

use carillon_protocol::{
    model::CommandCatalog,
    settings::{Change, Page},
};
mod card;
mod components;
pub mod contract;
mod embed;
mod prose;
mod run;
mod settings;

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub mod abi;

/// Defines a Wasm module's entry points so the provider's host invokes
/// `$handler`, a function of the same shape as [`handle`]. This library
/// defines none itself: the bundled module (`carillon-discord-bundled`)
/// exports [`handle`], and a custom module exports its own, which can
/// delegate to [`handle`] for what it does not change. Use it once, in a
/// `cdylib`.
#[macro_export]
macro_rules! export {
    ($handler:path) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            #[expect(unsafe_code, reason = "wasm export")]
            #[unsafe(no_mangle)]
            extern "C" fn input(length: u32) -> u32 {
                $crate::abi::input(length)
            }

            #[expect(unsafe_code, reason = "wasm export")]
            #[unsafe(no_mangle)]
            extern "C" fn output() -> u32 {
                $crate::abi::output()
            }

            #[expect(unsafe_code, reason = "wasm export")]
            #[unsafe(no_mangle)]
            extern "C" fn invoke() -> u32 {
                $crate::abi::invoke(|host, invocation| $handler(host, invocation))
            }
        };
    };
}

use contract::{HostReply, HostRequest, Intent, Invocation, Notice, Outcome};

/// Native operations the provider performs on the module's behalf. Each call
/// answers with what the provider confirmed, or the reason it could not, in
/// words meant for the user.
pub trait HostCalls {
    /// The provider's answer; never [`HostReply::Error`], which is the error.
    ///
    /// # Errors
    ///
    /// Returns the user-facing reason the operation was refused or failed.
    fn call(&mut self, request: HostRequest) -> Result<HostReply, String>;

    /// The page of the routed channel the interaction happened in.
    ///
    /// # Errors
    ///
    /// See [`HostCalls::call`].
    fn settings(&mut self) -> Result<(Page, Option<Notice>), String> {
        page(self.call(HostRequest::Settings)?)
    }

    /// Applies a change to a line of that page, then reads the page again,
    /// with what became of the change when it did not apply as asked.
    ///
    /// # Errors
    ///
    /// See [`HostCalls::call`].
    fn change_setting(&mut self, change: Change) -> Result<(Page, Option<Notice>), String> {
        page(self.call(HostRequest::ChangeSetting(change))?)
    }

    /// # Errors
    ///
    /// See [`HostCalls::call`].
    fn commands(&mut self) -> Result<CommandCatalog, String> {
        match self.call(HostRequest::Commands)? {
            HostReply::Commands(catalog) => Ok(catalog),
            _ => Err(UNEXPECTED.to_owned()),
        }
    }

    /// Sends one of the runtime's commands as the person's message.
    ///
    /// # Errors
    ///
    /// See [`HostCalls::call`].
    fn submit_command(&mut self, text: String) -> Result<(), String> {
        match self.call(HostRequest::Submit {
            text,
            intent: Intent::Command,
        })? {
            HostReply::Submitted => Ok(()),
            _ => Err(UNEXPECTED.to_owned()),
        }
    }
}

const UNEXPECTED: &str = "The provider gave an unexpected answer.";

fn page(reply: HostReply) -> Result<(Page, Option<Notice>), String> {
    match reply {
        HostReply::Page { page, notice } => Ok((page, notice)),
        _ => Err(UNEXPECTED.to_owned()),
    }
}

/// Handle one invocation. Failures are reported, never panicked, because the
/// provider treats a trap as a module fault.
pub fn handle(host: &mut impl HostCalls, invocation: Invocation) -> Outcome {
    match invocation {
        Invocation::Describe => Outcome::Description(contract::Description {
            contract: contract::CONTRACT,
            commands: vec![settings::definition(), run::definition()],
        }),
        // A command's components carry its name before the first `:`.
        Invocation::Interaction(interaction) => match interaction.name.split(':').next() {
            Some(run::COMMAND) => run::interact(host, &interaction),
            _ => settings::interact(host, &interaction),
        },
        Invocation::Card(request) => Outcome::Card(card::render(&request)),
    }
}
