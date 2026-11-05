//! Typing while a job's turn runs, and not while it waits in its runtime.

use super::*;

#[tokio::test]
async fn disabled_cards_still_type_name_no_jobs_and_leave_records_alone() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    std::fs::write(
        &records,
        serde_json::to_vec(&json!({
            "running": {"channel_id": "300", "message_id": "555"},
            "unnamed": {"channel_id": "302"},
        }))?,
    )?;
    let before = std::fs::read(&records)?;
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let make = tools(None, &[("bash", "make")], &[]);
    discord.record(vec![running("running", "300", accepted, Some(make))]);
    // Cards are turned off and the provider restarted; typing is its own setting.
    let mut configured = config(true);
    configured.activity = false;
    let mut provider = discord.provider(discord.http(false), configured, &records);
    discord
        .wait_for(&Method::POST, "/channels/300/typing", 1)
        .await?;
    // An unreachable core changes nothing.
    discord.unavailable();
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let done = tools(None, &[], &[("bash", "make", true)]);
    discord.record(vec![ended(
        "running",
        "300",
        accepted,
        TurnState::Completed,
        Some(done),
        None,
    )]);
    discord.deliver(answer(1, "running", "running", TurnState::Completed));
    discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    provider.stop().await?;
    // The answer goes out plainly; no card is made, edited or removed,
    // no job is read by name, and the records stay as they were.
    let messages = discord.matching(&Method::POST, "/channels/300/messages");
    assert_eq!(messages.len(), 1, "{:#?}", discord.seen());
    assert_eq!(messages[0].body["content"], "answer");
    assert!(!messages[0].is_card());
    assert!(discord.matching(&Method::PATCH, "/channels").is_empty());
    assert!(discord.matching(&Method::DELETE, "/channels").is_empty());
    assert!(discord.named().is_empty());
    assert_eq!(std::fs::read(&records)?, before);
    Ok(())
}

#[tokio::test]
async fn a_turn_waiting_in_its_runtime_shows_no_typing_until_it_runs() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now() - 60;
    let waiting = TurnActivity {
        phase: ActivityPhase::Queued,
        ..tools(None, &[], &[])
    };
    discord.record(vec![running("waiting", "300", accepted, Some(waiting))]);
    let mut configured = config(true);
    configured.activity = false;
    let mut provider = discord.provider(discord.http(false), configured, &records);
    discord.read_taken(1).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        discord
            .matching(&Method::POST, "/channels/300/typing")
            .is_empty()
    );
    let make = tools(None, &[("bash", "make")], &[]);
    discord.record(vec![running("waiting", "300", accepted, Some(make))]);
    discord
        .wait_for(&Method::POST, "/channels/300/typing", 1)
        .await?;
    provider.stop().await?;
    Ok(())
}

#[tokio::test]
async fn new_jobs_type_and_cards_draw_as_the_settings_serving_then_say() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let records = directory.path().join("activity-cards.json");
    let discord = Substitute::start(succeed()).await?;
    let accepted = now();
    let work = || tools(None, &[("bash", "make")], &[]);
    let first = running("first", "300", accepted, Some(work()));
    discord.record(vec![first.clone()]);
    // The provider starts without typing, drawing the default markers.
    let settings = crate::config::Settings::new(config(false));
    let (_delivering, monitor) = discord.serving(discord.http(false), settings.clone(), &records);
    let card = discord
        .wait_for(&Method::POST, "/channels/300/messages", 1)
        .await?;
    assert!(card[0].content().starts_with("⏳ Working"), "{card:#?}");
    // The operator then serves typing and other markers, and another job
    // starts.
    let mut next = with_markers()?;
    next.typing = true;
    let changed = settings
        .apply(
            || Ok(next),
            |_| std::future::ready(Ok(crate::module::tests::unchanged())),
        )
        .await
        .map_err(|refusal| anyhow::anyhow!("{refusal:?}"))?
        .changed;
    assert_eq!(changed, ["typing", "activity_markers"]);
    discord.record(vec![
        first,
        running("second", "301", accepted, Some(work())),
    ]);
    discord
        .wait_for(&Method::POST, "/channels/301/typing", 1)
        .await?;
    let card = discord
        .wait_for(&Method::POST, "/channels/301/messages", 1)
        .await?;
    assert!(
        card[0].content().starts_with("<a:working:123> Working"),
        "{card:#?}"
    );
    // The job followed before keeps the typing decision it started with.
    assert!(
        discord
            .matching(&Method::POST, "/channels/300/typing")
            .is_empty()
    );
    monitor.abort();
    Ok(())
}
