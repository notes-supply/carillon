use anyhow::{Context as _, Result};
use carillon_core::{AgentOptions, Core, CoreOptions, RuntimeProvider, WorkerStep};
use carillon_installation::config::AttachmentLimits;
use carillon_protocol::{
    model::{ExternalEvent, SelectedProvider},
    turns::TurnOutcome,
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn a_direct_message_conversation_keeps_one_turn_per_event_across_replays_and_restarts()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let options = CoreOptions {
        data_directory: directory.path().join("state"),
        agents: vec![AgentOptions {
            id: "agent".to_owned(),
            runtime: "runtime".to_owned(),
            resources: None,
        }],
        runtimes: vec![RuntimeProvider {
            id: "runtime".to_owned(),
            version: "test".to_owned(),
            url: "in-process".to_owned(),
            enabled: true,
        }],
        attachments: AttachmentLimits::default(),
        uploads: std::collections::BTreeMap::new(),
    };
    let interface = SelectedProvider {
        id: "discord".to_owned(),
        role: "interface".to_owned(),
        version: "test".to_owned(),
        url: "in-process".to_owned(),
    };
    let event = ExternalEvent {
        event_id: "500".to_owned(),
        agent_id: "agent".to_owned(),
        user_id: "200".to_owned(),
        audience_id: "discord-dm-900".to_owned(),
        channel_id: "dm".to_owned(),
        delivery_target_id: "900".to_owned(),
        feedback_target_id: "900:500".to_owned(),
        content: "original input".to_owned(),
        native: serde_json::Value::Null,
        attachments: None,
    };
    let core = Core::create(options.clone())?;
    let original = core.accept(&interface, &event).await?;
    let (one, two) = tokio::join!(
        core.accept(&interface, &event),
        core.accept(&interface, &event)
    );
    for replay in [one?, two?] {
        assert!(replay.duplicate);
        assert_eq!(replay.turn_id, original.turn_id);
    }
    core.storage_settled().await;
    drop(core);
    let core = Core::open(options)?;
    assert_eq!(core.recover_interrupted()?, 0);
    let changed = ExternalEvent {
        content: "replay tries to rewrite history".to_owned(),
        ..event.clone()
    };
    let replay = core.accept(&interface, &changed).await?;
    assert!(replay.duplicate);
    assert_eq!(replay.turn_id, original.turn_id);
    let second = ExternalEvent {
        event_id: "501".to_owned(),
        content: "new input".to_owned(),
        feedback_target_id: "900:501".to_owned(),
        ..event
    };
    let next = core.accept(&interface, &second).await?;
    assert_ne!(next.turn_id, original.turn_id);
    for input in ["original input", "new input"] {
        let mut request = None;
        assert_eq!(
            core.dispatch_next("runtime", &CancellationToken::new(), async |turn| {
                request = Some(turn);
                Ok(())
            })
            .await?,
            WorkerStep::Advanced
        );
        let request = request.context("no dispatched turn")?;
        assert_eq!(request.audience_id, "discord-dm-900");
        assert_eq!(request.channel_id, "dm");
        assert_eq!(request.input, input);
        core.settle(
            "runtime",
            &request.correlation_id,
            TurnOutcome::Answered {
                output: "answer".to_owned(),
                activity: None,
                files: Vec::new(),
            },
        )?;
    }
    assert_eq!(
        core.dispatch_next("runtime", &CancellationToken::new(), async |_| Ok(()))
            .await?,
        WorkerStep::Idle
    );
    core.storage_settled().await;
    Ok(())
}
