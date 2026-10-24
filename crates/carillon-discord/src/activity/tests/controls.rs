//! The control reactions on a job's message, as the monitor reads its
//! runtime's snapshots, beside the delivery loop that withdraws them before
//! the message's outcome shows.

use super::*;
use carillon_protocol::turns::{ControlAction, Silence};

/// A Unicode emoji as Twilight writes it into a reaction's path.
fn encoded(emoji: &str) -> String {
    use std::fmt::Write as _;
    emoji.bytes().fold(String::new(), |mut encoded, byte| {
        let _ = write!(encoded, "%{byte:02X}");
        encoded
    })
}

/// The bot's own reaction `emoji` on message 400 of channel 300.
fn own(method: &Method, emoji: &str) -> (Method, String) {
    (
        method.clone(),
        format!(
            "/channels/300/messages/400/reactions/{}/@me",
            encoded(emoji)
        ),
    )
}

/// The reactions Discord was asked for, in order.
fn reactions(discord: &Substitute) -> Vec<(Method, String)> {
    discord
        .seen()
        .into_iter()
        .filter(|seen| seen.path.contains("/reactions/"))
        .map(|seen| (seen.method, seen.path))
        .collect()
}

/// The job of message 400 in channel 300, its turn running as `activity`
/// says.
fn job_of_message(accepted_at: i64, activity: TurnActivity) -> Job {
    Job {
        feedback_target_id: Some("300:400".to_owned()),
        ..running("job", "300", accepted_at, Some(activity))
    }
}

/// A turn its runtime holds behind another, offering `controls`.
fn waiting(controls: &[ControlAction]) -> TurnActivity {
    TurnActivity {
        phase: ActivityPhase::Queued,
        controls: controls.to_vec(),
        ..tools(None, &[], &[])
    }
}

/// The monitor beside the delivery loop, with reactions, cards and typing
/// off.
fn provider(discord: &Substitute, records: &Path) -> Provider {
    let mut config = config(false);
    config.activity = false;
    let http = discord.http(false);
    let reactions = Reactions::new(http.clone(), crate::config::Settings::new(config));
    let (delivering, heard) = mpsc::unbounded_channel();
    let stopping = CancellationToken::new();
    let delivered = delivery_loop(
        discord.client.clone(),
        reactions.clone(),
        http,
        delivering,
        stopping.clone(),
    );
    let monitor = activity_monitor(
        discord.client.clone(),
        discord.module.clone(),
        reactions,
        CardRecords::open(records.to_path_buf()),
        heard,
        stopping.clone(),
    );
    Provider {
        monitor: tokio::spawn(monitor),
        delivering: tokio::spawn(delivered),
        stopping,
    }
}

#[tokio::test]
async fn a_waiting_turn_offers_its_runtimes_controls_until_it_runs() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let offering = waiting(&[ControlAction::Steer, ControlAction::Stop]);
    discord.record(vec![job_of_message(accepted, offering)]);
    let mut provider = provider(&discord, &records);
    discord
        .wait_for(&Method::PUT, "/channels/300/messages/400/reactions/", 2)
        .await?;
    // Running, the turn offers nothing: what was offered goes.
    let make = tools(None, &[("bash", "make")], &[]);
    discord.record(vec![job_of_message(accepted, make)]);
    discord
        .wait_for(&Method::DELETE, "/channels/300/messages/400/reactions/", 2)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    assert_eq!(
        reactions(&discord),
        [
            own(&Method::PUT, "⏩"),
            own(&Method::PUT, "🛑"),
            own(&Method::DELETE, "⏩"),
            own(&Method::DELETE, "🛑"),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_silent_outcome_withdraws_the_controls_and_sends_nothing() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let all = [
        ControlAction::Steer,
        ControlAction::Interrupt,
        ControlAction::Stop,
    ];
    discord.record(vec![job_of_message(accepted, waiting(&all))]);
    let mut provider = provider(&discord, &records);
    discord
        .wait_for(&Method::PUT, "/channels/300/messages/400/reactions/", 3)
        .await?;
    // The runtime steered the message into the turn before it.
    let steered = ended("job", "300", accepted, TurnState::Completed, None, None);
    discord.record(vec![Job {
        feedback_target_id: Some("300:400".to_owned()),
        ..steered
    }]);
    discord.deliver(Delivery {
        content: String::new(),
        silence: Some(Silence::Steered),
        ..answer(1, "job", "job", TurnState::Completed)
    });
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "↪️").1, 1)
        .await?;
    provider.stop().await?;
    let seen = reactions(&discord);
    assert_eq!(seen.last(), Some(&own(&Method::PUT, "↪️")), "{seen:#?}");
    let withdrawn = ["⏩", "⏭️", "🛑"].map(|emoji| own(&Method::DELETE, emoji));
    assert_eq!(
        seen[..3],
        ["⏩", "⏭️", "🛑"].map(|emoji| own(&Method::PUT, emoji))
    );
    assert!(
        withdrawn
            .iter()
            .all(|withdrawal| seen[3..seen.len() - 1].contains(withdrawal))
    );
    assert!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .is_empty()
    );
    Ok(())
}
