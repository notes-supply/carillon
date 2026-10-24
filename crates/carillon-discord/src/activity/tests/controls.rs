//! The control reactions on a job's message, as the monitor reads its
//! runtime's snapshots, beside the delivery loop that withdraws them before
//! the message's outcome shows.

use super::*;
use crate::config::Settings;
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

/// The settings the provider starts with: cards and typing off.
fn controlled() -> DiscordConfig {
    let mut config = config(false);
    config.activity = false;
    config
}

/// The monitor beside the delivery loop, serving `controlled` settings.
fn provider(discord: &Substitute, records: &Path) -> Provider {
    serving(discord, records, &Settings::new(controlled()))
}

/// The monitor beside the delivery loop, serving `settings`.
fn serving(discord: &Substitute, records: &Path, settings: &Settings) -> Provider {
    following(
        discord,
        records,
        &Reactions::new(discord.http(false), settings.clone()),
    )
}

/// The monitor beside the delivery loop, both with `reactions`.
fn following(discord: &Substitute, records: &Path, reactions: &Reactions) -> Provider {
    let (http, reactions) = (reactions.http().clone(), reactions.clone());
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
    // Running, the turn offers nothing: every control goes.
    let make = tools(None, &[("bash", "make")], &[]);
    discord.record(vec![job_of_message(accepted, make)]);
    discord
        .wait_for(&Method::DELETE, "/channels/300/messages/400/reactions/", 3)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    // An earlier process may have left the control it does not offer.
    assert_eq!(
        reactions(&discord),
        [
            own(&Method::DELETE, "⏭️"),
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
    assert_eq!(
        seen[..3],
        ["⏩", "⏭️", "🛑"].map(|emoji| own(&Method::PUT, emoji))
    );
    // Every control goes before the outcome shows.
    let outcome = seen
        .iter()
        .position(|seen| *seen == own(&Method::PUT, "↪️"))
        .context("no outcome reaction")?;
    assert!(
        ["⏩", "⏭️", "🛑"]
            .map(|emoji| own(&Method::DELETE, emoji))
            .iter()
            .all(|withdrawal| seen[3..outcome].contains(withdrawal)),
        "{seen:#?}"
    );
    assert!(
        discord
            .matching(&Method::POST, "/channels/300/messages")
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_restarted_provider_removes_the_controls_an_earlier_one_left() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let make = tools(None, &[("bash", "make")], &[]);
    discord.record(vec![job_of_message(accepted, make)]);
    let mut provider = provider(&discord, &records);
    // A turn offering no control leaves the message as it is until its
    // outcome, which removes each control once.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(reactions(&discord), []);
    discord.record(vec![answered(accepted)]);
    discord.deliver(answer(1, "job", "job", TurnState::Completed));
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "✅").1, 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    assert_eq!(
        reactions(&discord),
        [
            own(&Method::DELETE, "⏩"),
            own(&Method::DELETE, "⏭️"),
            own(&Method::DELETE, "🛑"),
            own(&Method::DELETE, "👀"),
            own(&Method::PUT, "✅"),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn a_control_discord_did_not_add_is_added_at_the_next_read() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(Arc::new(|seen: &Seen, earlier| {
        if seen.method == Method::PUT && earlier == 0 {
            Reply::Status(500)
        } else {
            Reply::Success
        }
    }))
    .await?;
    discord.record(vec![job_of_message(
        now() - 60,
        waiting(&[ControlAction::Steer]),
    )]);
    let mut provider = provider(&discord, &records);
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "⏩").1, 2)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    assert_eq!(
        reactions(&discord),
        [
            own(&Method::DELETE, "⏭️"),
            own(&Method::DELETE, "🛑"),
            own(&Method::PUT, "⏩"),
            own(&Method::PUT, "⏩"),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn new_control_emoji_replace_the_ones_a_message_shows() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    discord.record(vec![job_of_message(
        now() - 60,
        waiting(&[ControlAction::Steer]),
    )]);
    let settings = Settings::new(controlled());
    let mut provider = serving(&discord, &records, &settings);
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "⏩").1, 1)
        .await?;
    steer_with_play(&settings).await?;
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "▶️").1, 1)
        .await?;
    provider.stop().await?;
    let seen = reactions(&discord);
    assert_eq!(
        seen[3..],
        [own(&Method::DELETE, "⏩"), own(&Method::PUT, "▶️")],
        "{seen:#?}"
    );
    Ok(())
}

/// Moves the steer control to ▶️ while `settings` serve.
async fn steer_with_play(settings: &Settings) -> anyhow::Result<()> {
    let mut next = controlled();
    "▶️".clone_into(&mut next.steer_emoji);
    settings
        .apply(
            || Ok(next),
            |_| std::future::ready(Ok(crate::module::tests::unchanged())),
        )
        .await
        .map_err(|refusal| anyhow::anyhow!("{refusal:?}"))?;
    Ok(())
}

#[tokio::test]
async fn an_old_control_discord_did_not_remove_is_removed_at_the_next_read() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let old = own(&Method::DELETE, "⏩");
    let discord = Substitute::start(Arc::new(move |seen: &Seen, earlier| {
        if seen.is(&old.0, &old.1) && earlier == 0 {
            Reply::Status(500)
        } else {
            Reply::Success
        }
    }))
    .await?;
    discord.record(vec![job_of_message(
        now() - 60,
        waiting(&[ControlAction::Steer]),
    )]);
    let settings = Settings::new(controlled());
    let mut provider = serving(&discord, &records, &settings);
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "⏩").1, 1)
        .await?;
    steer_with_play(&settings).await?;
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "▶️").1, 2)
        .await?;
    provider.stop().await?;
    let seen = reactions(&discord);
    // ⏩ stays one the message may show until Discord removes it.
    assert_eq!(
        seen[3..],
        [
            own(&Method::DELETE, "⏩"),
            own(&Method::PUT, "▶️"),
            own(&Method::DELETE, "⏩"),
            own(&Method::PUT, "▶️"),
        ],
        "{seen:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_control_added_while_the_outcome_shows_is_still_withdrawn() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let slow = Duration::from_millis(1_500);
    let discord = Substitute::start(Arc::new(move |seen: &Seen, _| {
        if seen.is(&Method::PUT, &own(&Method::PUT, "⏩").1) {
            Reply::Status(500)
        } else if seen.is(&Method::PUT, &own(&Method::PUT, "⏭️").1) {
            Reply::Delayed(slow)
        } else {
            Reply::Success
        }
    }))
    .await?;
    let accepted = now() - 60;
    let offering = waiting(&[ControlAction::Steer, ControlAction::Interrupt]);
    discord.record(vec![job_of_message(accepted, offering)]);
    let mut provider = provider(&discord, &records);
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "⏭️").1, 1)
        .await?;
    // The turn is answered while Discord is still adding ⏭️, in a pass
    // that failed to add ⏩.
    let answered = ended("job", "300", accepted, TurnState::Completed, None, None);
    discord.record(vec![Job {
        feedback_target_id: Some("300:400".to_owned()),
        ..answered
    }]);
    discord.deliver(answer(1, "job", "job", TurnState::Completed));
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "✅").1, 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    // Each control's last change Discord applied removes it.
    let seen = discord.seen();
    for emoji in ["⏩", "⏭️", "🛑"] {
        let applied = |method: &Method| {
            seen.iter()
                .filter(|seen| seen.is(method, &own(method, emoji).1))
                .map(|seen| {
                    seen.at
                        + if *method == Method::PUT && emoji == "⏭️" {
                            slow
                        } else {
                            Duration::ZERO
                        }
                })
                .max()
        };
        assert!(
            applied(&Method::PUT) < applied(&Method::DELETE),
            "{emoji} left on the message: {:#?}",
            reactions(&discord)
        );
    }
    Ok(())
}

/// The job of message 400, its turn answered, as core records it until the
/// answer's delivery is.
fn answered(accepted_at: i64) -> Job {
    Job {
        feedback_target_id: Some("300:400".to_owned()),
        ..ended("job", "300", accepted_at, TurnState::Completed, None, None)
    }
}

/// Reactions serving `settings` that admitted message 400.
async fn admitted(discord: &Substitute, settings: &Settings) -> Reactions {
    let feedback = Reactions::new(discord.http(false), settings.clone());
    let message = carillon_protocol::model::ExternalEvent {
        event_id: "400".to_owned(),
        agent_id: "default".to_owned(),
        user_id: "200".to_owned(),
        audience_id: "discord-100-300".to_owned(),
        delivery_target_id: "300".to_owned(),
        feedback_target_id: "300:400".to_owned(),
        content: "hello".to_owned(),
        native: Value::Null,
        attachments: None,
    };
    feedback.admission(&message, &settings.current()).await;
    feedback
}

/// Message 400's reactions when it is answered without offering a control.
fn answered_without_controls() -> [(Method, String); 3] {
    [
        own(&Method::PUT, "👀"),
        own(&Method::DELETE, "👀"),
        own(&Method::PUT, "✅"),
    ]
}

#[tokio::test]
async fn an_answered_message_admitted_here_without_controls_costs_no_control_calls()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let feedback = admitted(&discord, &Settings::new(controlled())).await;
    let make = tools(None, &[("bash", "make")], &[]);
    discord.record(vec![job_of_message(accepted, make)]);
    let mut provider = following(&discord, &records, &feedback);
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    discord.record(vec![answered(accepted)]);
    discord.deliver(answer(1, "job", "job", TurnState::Completed));
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "✅").1, 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    provider.stop().await?;
    assert_eq!(reactions(&discord), answered_without_controls());
    Ok(())
}

#[tokio::test]
async fn a_fast_answer_read_after_its_outcome_costs_no_control_calls() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let feedback = admitted(&discord, &Settings::new(config(false))).await;
    discord.record(vec![answered(now() - 60)]);
    let mut provider = following(&discord, &records, &feedback);
    discord.read_taken(0).await?;
    // The job ends between two reads, and the monitor's next read answers
    // only once the outcome shows.
    let held = discord.hold_reads().await;
    discord.deliver(answer(1, "job", "job", TurnState::Completed));
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "✅").1, 1)
        .await?;
    drop(held);
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    assert_eq!(reactions(&discord), answered_without_controls());
    Ok(())
}

#[tokio::test]
async fn a_fast_answer_read_before_its_outcome_costs_no_control_calls() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let feedback = admitted(&discord, &Settings::new(config(false))).await;
    discord.record(vec![sent(answered(now() - 60))]);
    let mut provider = following(&discord, &records, &feedback);
    // The monitor reads the job settled while core is still recording its
    // delivery, before the outcome shows.
    let held = discord.hold_recording().await;
    discord.deliver(answer(1, "job", "job", TurnState::Completed));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !discord.named().iter().flatten().any(|job| job == "job") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("the monitor never read the job")?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(held);
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "✅").1, 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    assert_eq!(reactions(&discord), answered_without_controls());
    Ok(())
}

#[tokio::test]
async fn an_answer_delivered_after_its_job_is_no_longer_followed_costs_no_control_calls()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let feedback = admitted(&discord, &Settings::new(controlled())).await;
    let make = tools(None, &[("bash", "make")], &[]);
    discord.record(vec![job_of_message(accepted, make)]);
    let mut provider = following(&discord, &records, &feedback);
    discord.read_taken(0).await?;
    // The turn ends, so the next read no longer lists the job at work, and
    // its answer is delivered only after that.
    discord.record(vec![answered(accepted)]);
    let begun = discord.reads();
    discord.read_taken(begun).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    discord.deliver(answer(1, "job", "job", TurnState::Completed));
    discord
        .wait_for(&Method::PUT, &own(&Method::PUT, "✅").1, 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    provider.stop().await?;
    assert_eq!(reactions(&discord), answered_without_controls());
    Ok(())
}
