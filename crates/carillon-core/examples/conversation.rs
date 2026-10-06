//! A complete durable conversation with no HTTP, Discord or model account.
use anyhow::{Context as _, Result, ensure};
use carillon_core::{Core, CoreOptions, Lane, RuntimeProvider, WorkerStep};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::model::{
    DeliveryAck, DeliveryOutcome, ExternalEvent, RuntimeTurnResponse, SelectedProvider, TurnState,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    conversation().await
}

async fn conversation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let resources = directory.path().join("resources");
    std::fs::create_dir(&resources)?;
    std::fs::write(resources.join("SYSTEM.md"), "Captured instructions")?;
    let core = Core::create(CoreOptions {
        data_directory: directory.path().join("state"),
        agent_id: "example".to_owned(),
        resource_root: resources.clone(),
        default_runtime: "echo".to_owned(),
        runtimes: vec![RuntimeProvider {
            id: "echo".to_owned(),
            version: "example".to_owned(),
            url: "in-process".to_owned(),
            enabled: true,
        }],
        attachments: AttachmentLimits::default(),
    })?;
    // An application authenticates the caller before supplying this interface identity.
    let interface = SelectedProvider {
        id: "example".to_owned(),
        role: "interface".to_owned(),
        version: "example".to_owned(),
        url: "in-process".to_owned(),
    };
    let event = ExternalEvent {
        event_id: "message-1".to_owned(),
        agent_id: "example".to_owned(),
        user_id: "reader".to_owned(),
        audience_id: "conversation-1".to_owned(),
        delivery_target_id: "conversation-1".to_owned(),
        feedback_target_id: "message-1".to_owned(),
        content: "Hello building blocks".to_owned(),
        native: serde_json::json!({"origin":{"message_id":"original"}}),
        attachments: None,
    };
    let receipt = core.accept(&interface, &event).await?;
    ensure!(
        core.accept(
            &interface,
            &ExternalEvent {
                native: serde_json::json!({"changed":true}),
                ..event.clone()
            }
        )
        .await?
        .duplicate,
        "duplicate acceptance changed the turn"
    );
    std::fs::write(resources.join("SYSTEM.md"), "Changed after acceptance")?;
    // This example never stops; a service cancels the token as it shuts down.
    let stopping = CancellationToken::new();
    let step = core
        .run_next("echo", Lane::Messages, &stopping, async |request| {
            // Opaque interface data is captured on acceptance, not supplied by a duplicate.
            assert_eq!(request.native, event.native);
            // This deterministic test runtime is deliberately not Pi. The snapshot is real.
            let captured = std::fs::read_to_string(
                std::path::Path::new(&request.resource_root).join("SYSTEM.md"),
            )
            .map_err(|error| carillon_core::TurnFailure::Uncertain(error.into()))?;
            Ok(RuntimeTurnResponse {
                correlation_id: request.correlation_id,
                output: format!("{} — {captured}", request.input),
                follow_up: None,
                unanswered_left: false,
            })
        })
        .await?;
    ensure!(step == WorkerStep::Advanced, "no turn executed");
    let delivery = core
        .claim_delivery("example", "instance-1")?
        .context("missing delivery")?;
    ensure!(
        delivery.content.ends_with("Captured instructions"),
        "accepted resources changed"
    );
    ensure!(
        core.inspect_turn(&receipt.turn_id)?
            .context("missing turn")?
            .state
            == TurnState::Completed,
        "turn did not complete"
    );
    core.acknowledge_delivery(
        "example",
        "instance-1",
        delivery.id,
        &DeliveryAck {
            native_ref: Some("example-reply".to_owned()),
            outcome: DeliveryOutcome::Delivered,
        },
    )?;
    ensure!(
        core.claim_delivery("example", "instance-1")?.is_none(),
        "acknowledged output reappeared"
    );
    println!(
        "{}",
        serde_json::json!({"state":"completed", "delivery":"delivered", "output":delivery.content})
    );
    Ok(())
}

#[tokio::test]
async fn example_uses_the_public_core_and_real_sqlite() -> Result<()> {
    conversation().await
}
