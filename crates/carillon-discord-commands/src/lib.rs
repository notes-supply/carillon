//! The replaceable Discord command module: definitions, handlers, and
//! presentation for the commands the Discord provider serves. The provider
//! keeps the gateway connection, admission, and every Discord or core call;
//! this module only decides what to show and what to ask for.

use carillon_protocol::model::ModelScope as Scope;
use carillon_protocol::models::{ModelState, Selection};
pub mod contract;
mod model;

#[cfg(target_arch = "wasm32")]
mod abi;

use contract::{HostRequest, Invocation, Outcome};

/// Native operations the provider performs on the module's behalf. Each call
/// answers with the runtime's confirmed model state or the reason it could
/// not, in words meant for the user.
pub trait HostCalls {
    /// # Errors
    ///
    /// Returns the user-facing reason the operation was refused or failed.
    fn call(&mut self, request: HostRequest) -> Result<ModelState, String>;

    /// # Errors
    ///
    /// See [`HostCalls::call`].
    fn model_state(&mut self) -> Result<ModelState, String> {
        self.call(HostRequest::ModelState)
    }

    /// # Errors
    ///
    /// See [`HostCalls::call`].
    fn set_model(
        &mut self,
        scope: Scope,
        selection: Option<Selection>,
    ) -> Result<ModelState, String> {
        self.call(HostRequest::SetModel {
            scope,
            selection,
            expected: None,
        })
    }
}

/// Handle one invocation. Failures are reported, never panicked, because the
/// provider treats a trap as a module fault.
pub fn handle(host: &mut impl HostCalls, invocation: Invocation) -> Outcome {
    match invocation {
        Invocation::Describe => Outcome::Description(contract::Description {
            contract: contract::CONTRACT,
            commands: vec![model::definition()],
        }),
        Invocation::Interaction(interaction) => model::interact(host, &interaction),
    }
}
