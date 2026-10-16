use anyhow::{Context as _, Result};
use carillon_core::{AgentOptions, Core, CoreOptions, Lane, RuntimeProvider, WorkerStep};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::model::{
    ExternalEvent, RuntimeTurnRequest, RuntimeTurnResponse, SelectedProvider, TurnState,
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

fn interface() -> SelectedProvider {
    SelectedProvider {
        id: "discord".to_owned(),
        role: "interface".to_owned(),
        version: "test".to_owned(),
        url: "in-process".to_owned(),
    }
}

const EXPLANATION: &str = "This conversation ran on old, which no longer runs agent.";

async fn queued_cutover(agent_runtime: Option<&str>, former_enabled: Option<bool>) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut options = options(directory.path());
    let core = Core::create(options.clone())?;
    let receipt = core.accept(&interface(), &event()).await?;
    core.storage_settled().await;
    drop(core);
    if let Some(runtime) = agent_runtime {
        runtime.clone_into(&mut options.agents[0].runtime);
    } else {
        options.agents.clear();
    }
    if let Some(enabled) = former_enabled {
        options.runtimes[0].enabled = enabled;
    } else {
        options.runtimes.remove(0);
    }
    let core = Core::open(options)?;
    assert_eq!(core.recover_interrupted()?, 1);
    let turn = core
        .inspect_turn(&receipt.turn_id)?
        .context("missing turn")?;
    assert_eq!(turn.state, TurnState::Interrupted);
    assert_eq!(turn.error.as_deref(), Some(EXPLANATION));
    let notice = core
        .claim_delivery("discord", "instance")?
        .context("missing stranded notice")?;
    assert_eq!(notice.turn_id, receipt.turn_id);
    assert_eq!(notice.turn_state, TurnState::Interrupted);
    assert_eq!(notice.content, EXPLANATION);
    assert_eq!(core.recover_interrupted()?, 0);
    assert_eq!(core.turn_counts()?.pending, 0);
    core.set_runtimes(vec![RuntimeProvider {
        id: "old".to_owned(),
        version: "test".to_owned(),
        url: "in-process".to_owned(),
        enabled: true,
    }])?;
    for lane in [Lane::Messages, Lane::Observations] {
        assert_eq!(
            core.run_next("old", lane, &CancellationToken::new(), async |_| {
                unreachable!("settled stranded turn was retried")
            })
            .await?,
            WorkerStep::Idle
        );
    }
    core.storage_settled().await;
    Ok(())
}

#[tokio::test]
async fn queued_removed_agent_is_interrupted_without_dispatch_or_retry() -> Result<()> {
    queued_cutover(None, Some(true)).await
}

#[tokio::test]
async fn queued_removed_agent_on_disabled_runtime_is_interrupted_at_startup() -> Result<()> {
    queued_cutover(None, Some(false)).await
}

#[tokio::test]
async fn queued_removed_agent_on_removed_runtime_is_interrupted_at_startup() -> Result<()> {
    queued_cutover(None, None).await
}

#[tokio::test]
async fn queued_moved_agent_on_disabled_runtime_is_interrupted_at_startup() -> Result<()> {
    queued_cutover(Some("new"), Some(false)).await
}

#[tokio::test]
async fn queued_moved_agent_on_removed_runtime_is_interrupted_at_startup() -> Result<()> {
    queued_cutover(Some("new"), None).await
}

async fn stranded_admission(former_enabled: Option<bool>) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut options = options(directory.path());
    let core = Core::create(options.clone())?;
    let first = core.accept(&interface(), &event()).await?;
    let original = core.inspect_turn(&first.turn_id)?.context("missing turn")?;
    core.storage_settled().await;
    drop(core);
    "new".clone_into(&mut options.agents[0].runtime);
    options.agents[0].resources = Some(directory.path().join("missing-resources"));
    let core = Core::open(options.clone())?;
    if let Some(enabled) = former_enabled {
        options.runtimes[0].enabled = enabled;
    } else {
        options.runtimes.remove(0);
    }
    core.set_runtimes(options.runtimes)?;
    let later = ExternalEvent {
        event_id: "later".to_owned(),
        ..event()
    };
    let receipt = core.accept(&interface(), &later).await?;
    assert!(!receipt.duplicate);
    assert_eq!(receipt.state, TurnState::Interrupted);
    let turn = core
        .inspect_turn(&receipt.turn_id)?
        .context("missing turn")?;
    assert_eq!(turn.state, TurnState::Interrupted);
    assert_eq!(
        turn.error.as_deref(),
        Some(
            "This conversation ran on old, which no longer runs agent. Your next message starts a new conversation on new."
        )
    );
    assert_eq!(turn.providers, original.providers);
    assert_eq!(turn.resource_snapshot_id, original.resource_snapshot_id);
    let notice = core
        .claim_turn_delivery("discord", "instance", &receipt.turn_id)?
        .context("missing immediate notice")?;
    assert_eq!(notice.turn_state, TurnState::Interrupted);
    assert_eq!(
        notice.content,
        "This conversation ran on old, which no longer runs agent. Your next message starts a new conversation on new."
    );
    core.storage_settled().await;
    Ok(())
}

#[tokio::test]
async fn stranded_message_is_accepted_when_former_runtime_is_disabled_live() -> Result<()> {
    stranded_admission(Some(false)).await
}

#[tokio::test]
async fn stranded_message_is_accepted_when_former_runtime_is_removed_live() -> Result<()> {
    stranded_admission(None).await
}

#[tokio::test]
async fn concurrent_stranded_duplicates_return_the_first_receipt() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut options = options(directory.path());
    let core = Core::create(options.clone())?;
    core.accept(&interface(), &event()).await?;
    core.storage_settled().await;
    drop(core);
    "new".clone_into(&mut options.agents[0].runtime);
    options.runtimes.clear();
    let core = Core::open(options)?;
    let interface = interface();
    let later = ExternalEvent {
        event_id: "later".to_owned(),
        ..event()
    };
    let (one, two) = tokio::join!(
        core.accept(&interface, &later),
        core.accept(&interface, &later)
    );
    let (one, two) = (one?, two?);
    assert_ne!(one.duplicate, two.duplicate);
    assert_eq!(one.turn_id, two.turn_id);
    assert_eq!(one.state, TurnState::Interrupted);
    assert_eq!(two.state, one.state);
    assert_eq!(core.recent_turns(Some("later"), 10)?.len(), 1);
    assert_eq!(core.delivery_counts()?.pending, 1);
    let duplicate = core.accept(&interface, &later).await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn_id, one.turn_id);
    assert_eq!(duplicate.state, one.state);
    core.storage_settled().await;
    Ok(())
}

#[tokio::test]
async fn moved_agent_strands_once_then_runs_the_same_channel_on_its_current_runtime() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let mut options = options(directory.path());
    let core = Core::create(options.clone())?;
    let interface = interface();
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
    "new".clone_into(&mut options.agents[0].runtime);
    let core = Core::open(options)?;
    assert_eq!(core.recover_interrupted()?, 2);
    let later = ExternalEvent {
        event_id: "later".to_owned(),
        ..event.clone()
    };
    core.accept(&interface, &later).await?;
    assert!(core.accept(&interface, &queued).await?.duplicate);
    for runtime in ["old", "new"] {
        assert_eq!(
            core.run_next(runtime, Lane::Messages, &stopping, async |_| {
                unreachable!("the notice must not dispatch to either runtime")
            })
            .await?,
            WorkerStep::Idle
        );
    }
    let explanation = "This conversation ran on old, which no longer runs agent.";
    let observation = "This conversation ran on old, which no longer runs agent.\nWork already started there will not be reported here. Actions already taken may have had effects.";
    for (state, content) in [
        (TurnState::Completed, "answer"),
        (TurnState::Interrupted, observation),
        (TurnState::Interrupted, explanation),
        (
            TurnState::Interrupted,
            "This conversation ran on old, which no longer runs agent. Your next message starts a new conversation on new.",
        ),
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
    for event_id in ["next", "still-new"] {
        let receipt = core
            .accept(
                &interface,
                &ExternalEvent {
                    event_id: event_id.to_owned(),
                    ..event.clone()
                },
            )
            .await?;
        assert_eq!(receipt.state, TurnState::Queued);
        assert_eq!(
            core.run_next("new", Lane::Messages, &stopping, async |request| {
                assert_eq!(request.correlation_id, receipt.turn_id);
                assert_eq!(request.audience_id, event.audience_id);
                Ok(answer(request))
            })
            .await?,
            WorkerStep::Advanced
        );
        assert_eq!(
            core.inspect_turn(&receipt.turn_id)?
                .context("missing turn")?
                .state,
            TurnState::Completed
        );
    }
    let duplicate = core.accept(&interface, &later).await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.state, TurnState::Interrupted);
    core.storage_settled().await;
    Ok(())
}

#[tokio::test]
async fn removed_agent_stays_stranded_in_the_same_channel() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut options = options(directory.path());
    let core = Core::create(options.clone())?;
    core.accept(&interface(), &event()).await?;
    core.storage_settled().await;
    drop(core);
    options.agents.clear();
    let core = Core::open(options)?;
    core.recover_interrupted()?;
    for event_id in ["notice", "still-stranded"] {
        let receipt = core
            .accept(
                &interface(),
                &ExternalEvent {
                    event_id: event_id.to_owned(),
                    ..event()
                },
            )
            .await?;
        assert_eq!(receipt.state, TurnState::Interrupted);
        let notice = core
            .claim_turn_delivery("discord", "instance", &receipt.turn_id)?
            .context("missing removed-agent notice")?;
        assert!(notice.content.contains("no longer runs agent"));
        assert!(!notice.content.contains("Your next message"));
        for runtime in ["old", "new"] {
            assert_eq!(
                core.run_next(
                    runtime,
                    Lane::Messages,
                    &CancellationToken::new(),
                    async |_| { unreachable!("removed agent received work") }
                )
                .await?,
                WorkerStep::Idle
            );
        }
    }
    core.storage_settled().await;
    Ok(())
}
