//! Durable conversation operations, independently usable with SQLite.
//! See `examples/conversation.rs` for an HTTP-free caller.
pub mod application;
mod attachments;
mod db;
mod engine;
mod intake;
mod resources;
mod runtime_client;

pub use attachments::Reconciled;
pub use engine::{
    AgentOptions, Core, CoreError, CoreOptions, Lane, Reported, RuntimeProvider, TurnFailure,
    WorkerStep,
};
pub use runtime_client::RuntimeClient;

/// Fails unless `result` is a refusal whose message, causes included,
/// mentions `reason`.
#[cfg(test)]
fn assert_refused<T, E: Into<anyhow::Error>>(
    result: Result<T, E>,
    reason: &str,
) -> anyhow::Result<()> {
    let Err(error) = result else {
        anyhow::bail!("accepted what should be refused: {reason}");
    };
    let error = format!("{:#}", error.into());
    anyhow::ensure!(
        error.contains(reason),
        "refused for another reason: {error}"
    );
    Ok(())
}
