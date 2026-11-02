//! A complete durable conversation with no HTTP, Discord or model account.
use anyhow::{Context as _, Result, ensure};
use carillon_core::{AgentOptions, Core, CoreOptions, RuntimeProvider, Settled, WorkerStep};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::{
    model::{
        DeliveryAck, DeliveryOutcome, ExternalEvent, RuntimeTurnRequest, SelectedProvider,
        TurnState,
    },
    turns::TurnOutcome,
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
        agents: vec![AgentOptions {
            id: "example".to_owned(),
            runtime: "echo".to_owned(),
            resources: Some(resources.clone()),
        }],
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
        channel_id: "channel-1".to_owned(),
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
    let mut handed = None;
    let step = core
        .dispatch_next("echo", &stopping, async |request| {
            // Opaque interface data is captured on acceptance, not supplied by a duplicate.
            assert_eq!(request.native, event.native);
            handed = Some(request);
            Ok(())
        })
        .await?;
    ensure!(step == WorkerStep::Advanced, "no turn handed over");
    // The runtime holds the turn, and settles it once it has answered.
    let request = handed.context("no turn handed over")?;
    ensure!(
        core.settle("echo", &request.correlation_id, echo(&request)?)? == Settled::Recorded,
        "the answer was not recorded"
    );
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

/// This deterministic test runtime is deliberately not Pi: it answers with
/// the input and the instructions of the turn's resource snapshot, which is real.
fn echo(request: &RuntimeTurnRequest) -> Result<TurnOutcome> {
    let captured =
        std::fs::read_to_string(std::path::Path::new(&request.resource_root).join("SYSTEM.md"))?;
    Ok(TurnOutcome::Answered {
        output: format!("{} — {captured}", request.input),
        activity: None,
    })
}

#[tokio::test]
async fn example_uses_the_public_core_and_real_sqlite() -> Result<()> {
    conversation().await
}
