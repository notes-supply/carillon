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

#[tokio::test]
async fn runtime_cutover_observation_explains_unreported_work_and_possible_effects() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let parent = accept(&state, "start native work", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(json!({"after": 1})),
            ..completed(request)
        })
    })
    .await?;
    let reply = state
        .core
        .claim_turn_delivery("test-interface", "instance", &parent.turn_id)?
        .context("missing initial reply")?;
    state.core.database.acknowledge_delivery(
        "test-interface",
        reply.id,
        "instance",
        DeliveryOutcome::Delivered,
        None,
    )?;
    state.config.value.agents[0].runtime = "replacement".to_owned();
    state.config.value.providers[1].enabled = false;
    assert_eq!(configured(&state).core.recover_interrupted()?, 1);
    let notice = state
        .core
        .claim_delivery("test-interface", "instance")?
        .context("missing observation refusal")?;
    assert_eq!(
        notice.content,
        "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation.\nWork already started there will not be reported here. Actions already taken may have had effects."
    );
    assert_eq!(state.core.database.turn_counts()?.interrupted, 1);
    Ok(())
}

#[tokio::test]
async fn startup_recovers_running_messages_once_and_settles_stranded_observations() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "running", "conversation").await?;
    state
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
        .context("missing running message")?;
    let queued = accept(&state, "queued", "conversation").await?;
    accept(&state, "work", "other-conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(json!({"after": 1})),
            ..completed(request)
        })
    })
    .await?;
    let observation = state
        .core
        .database
        .claim(
            "test-runtime",
            Lane::Observations,
            &CancellationToken::new(),
        )?
        .context("missing running observation")?;
    state.config.value.agents.clear();
    state.config.value.providers.remove(1);
    let core = configured(&state).core;
    assert_eq!(core.recover_interrupted()?, 3);
    for (turn_id, content) in [
        (
            running.turn_id,
            "Carillon restarted during that turn. It was not run again. Send another message to continue in this conversation.",
        ),
        (
            queued.turn_id,
            "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation.",
        ),
        (
            observation.turn_id,
            "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation.\nWork already started there will not be reported here. Actions already taken may have had effects.",
        ),
    ] {
        let notice = core
            .claim_turn_delivery("test-interface", "instance", &turn_id)?
            .context("missing recovery notice")?;
        assert_eq!(notice.turn_state, TurnState::Interrupted);
        assert_eq!(notice.content, content);
    }
    assert_eq!(core.recover_interrupted()?, 0);
    assert_eq!(core.turn_counts()?.interrupted, 3);
    Ok(())
}
