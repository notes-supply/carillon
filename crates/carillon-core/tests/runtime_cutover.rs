use anyhow::{Context as _, Result};
use carillon_core::{
    AgentOptions, Core, CoreOptions, Lane, RuntimeProvider, TurnFailure, WorkerStep,
};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::model::{
    ExternalEvent, ModelQuery, RuntimeTurnRequest, RuntimeTurnResponse, SelectedProvider, TurnState,
};
use tokio_util::sync::CancellationToken;

fn options(root: &std::path::Path) -> CoreOptions {
    CoreOptions {
        data_directory: root.join("data"),
        agents: vec![AgentOptions {
            id: "agent".to_owned(),
            runtime: "old".to_owned(),
            resources: None,
        }],
        runtimes: ["old", "new"]
            .map(|id| RuntimeProvider {
                id: id.to_owned(),
                version: "test".to_owned(),
                url: "in-process".to_owned(),
                enabled: true,
            })
            .into(),
        attachments: AttachmentLimits::default(),
    }
}

fn event() -> ExternalEvent {
    ExternalEvent {
        event_id: "first".to_owned(),
        agent_id: "agent".to_owned(),
        user_id: "user".to_owned(),
        audience_id: "existing".to_owned(),
        delivery_target_id: "conversation".to_owned(),
        feedback_target_id: "message".to_owned(),
        content: "hello".to_owned(),
        native: serde_json::Value::Null,
        attachments: None,
    }
}

fn answer(request: RuntimeTurnRequest) -> RuntimeTurnResponse {
    RuntimeTurnResponse {
        correlation_id: request.correlation_id,
        output: "answer".to_owned(),
        follow_up: None,
        unanswered_left: false,
        attachments_left: false,
        activity: None,
    }
}

#[tokio::test]
async fn changed_agent_runtime_explains_old_conversations_without_dispatching() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut options = options(directory.path());
    let core = Core::create(options.clone())?;
    let interface = SelectedProvider {
        id: "discord".to_owned(),
        role: "interface".to_owned(),
        version: "test".to_owned(),
        url: "in-process".to_owned(),
    };
    let event = event();
    core.accept(&interface, &event).await?;
    let stopping = CancellationToken::new();
    assert_eq!(
        core.run_next("old", Lane::Messages, &stopping, async |request| {
            let mut response = answer(request);
            response.follow_up = Some(serde_json::json!({"cursor": 1}));
            Ok(response)
        })
        .await?,
        WorkerStep::Advanced
    );
    let queued = ExternalEvent {
        event_id: "queued".to_owned(),
        ..event.clone()
    };
    core.accept(&interface, &queued).await?;
    core.storage_settled().await;
    drop(core);
    options.agents[0].runtime = "new".to_owned();
    let core = Core::open(options)?;
    let later = ExternalEvent {
        event_id: "later".to_owned(),
        ..event.clone()
    };
    core.accept(&interface, &later).await?;
    assert!(core.accept(&interface, &queued).await?.duplicate);
    let query = ModelQuery {
        agent_id: "agent".to_owned(),
        audience_id: Some("existing".to_owned()),
    };
    assert_eq!(core.runtime_for("discord", &query)?.id, "old");
    for lane in [Lane::Messages, Lane::Observations, Lane::Messages] {
        assert_eq!(
            core.run_next("old", lane, &stopping, async |_| {
                Err(TurnFailure::Uncertain(anyhow::anyhow!(
                    "old runtime received a turn after cutover"
                )))
            })
            .await?,
            WorkerStep::Advanced
        );
    }
    let explanation =
        "This conversation ran on old, which no longer runs agent. Start a new conversation.";
    for (state, content) in [
        (TurnState::Completed, "answer"),
        (TurnState::Interrupted, explanation),
        (TurnState::Interrupted, explanation),
        (TurnState::Interrupted, explanation),
    ] {
        let delivery = core
            .claim_delivery("discord", "instance")?
            .context("missing cutover explanation")?;
        assert_eq!(delivery.turn_state, state);
        assert_eq!(delivery.content, content);
        core.acknowledge_delivery(
            "discord",
            "instance",
            delivery.id,
            &carillon_protocol::model::DeliveryAck {
                native_ref: None,
                outcome: carillon_protocol::model::DeliveryOutcome::Delivered,
            },
        )?;
    }
    let fresh = core
        .accept(
            &interface,
            &ExternalEvent {
                event_id: "fresh".to_owned(),
                audience_id: "fresh".to_owned(),
                ..event
            },
        )
        .await?;
    assert_eq!(
        core.run_next("new", Lane::Messages, &stopping, async |request| Ok(
            answer(request)
        ))
        .await?,
        WorkerStep::Advanced
    );
    assert_eq!(
        core.inspect_turn(&fresh.turn_id)?
            .context("missing fresh turn")?
            .state,
        TurnState::Completed
    );
    core.storage_settled().await;
    Ok(())
}
