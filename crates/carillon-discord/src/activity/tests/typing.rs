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
