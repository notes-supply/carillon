use super::*;

#[tokio::test]
async fn runtime_cutover_interrupts_pinned_conversations() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let mut replacement = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    replacement.id = "replacement".to_owned();
    fs::write(
        directory.path().join("credentials/replacement.token"),
        "test-only",
    )?;
    state.config.value.providers.push(replacement);
    let first = accept(&state, "one", "conversation").await?;
    state.config.value.agents[0].runtime = "replacement".to_owned();
    state.core.database = Database::open(&state.config.value.data_dir.join("carillon.sqlite3"))?;
    let second = accept(&state, "two", "conversation").await?;
    let fresh = accept(&state, "fresh", "new-conversation").await?;
    run_next_turn(&state, "replacement", async |_, _, request| {
        assert_eq!(request.correlation_id, fresh.turn_id);
        Ok(completed(request))
    })
    .await?;
    for receipt in [&first, &second] {
        let inspected = state
            .core
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("missing turn")?;
        assert_eq!(inspected.providers[1]["id"], "test-runtime");
        run_next_turn(&state, "test-runtime", async |_, _, _| {
            unreachable!("stranded conversation reached its old runtime")
        })
        .await?;
        let notice = state
            .core
            .claim_turn_delivery("test-interface", "instance", &receipt.turn_id)?
            .context("missing cutover notice")?;
        assert_eq!(
            notice.content,
            "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation."
        );
    }
    let counts = state.core.database.turn_counts()?;
    assert_eq!(counts.completed, 1);
    assert_eq!(counts.interrupted, 2);
    Ok(())
}
