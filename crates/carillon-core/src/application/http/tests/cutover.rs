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
    assert_eq!(configured(&state).core.recover_interrupted()?, 1);
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
        assert_eq!(inspected.state, TurnState::Interrupted);
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

#[tokio::test]
async fn removed_agent_messages_keep_the_notice_and_original_receipt() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let original = accept(&state, "first", "conversation").await?;
    state.config.value.agents.clear();
    state.config.value.providers.remove(1);
    let later = accept_for(&state, "test-agent", "later", "conversation").await?;
    assert_eq!(later.state, TurnState::Interrupted);
    let notice = state
        .core
        .claim_turn_delivery("test-interface", "instance", &later.turn_id)?
        .context("missing removed-agent notice")?;
    assert_eq!(notice.turn_state, TurnState::Interrupted);
    assert_eq!(
        notice.content,
        "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation."
    );
    for (event_id, turn_id) in [("first", original.turn_id), ("later", later.turn_id)] {
        let duplicate = accept_for(&state, "test-agent", event_id, "conversation").await?;
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.turn_id, turn_id);
    }
    assert_eq!(
        accept_for(&state, "test-agent", "new", "new-conversation")
            .await
            .err()
            .context("unknown agent started a conversation")?
            .to_string(),
        "unknown agent"
    );
    Ok(())
}

#[tokio::test]
async fn stranded_admission_rolls_back_the_turn_when_its_notice_cannot_commit() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    accept(&state, "first", "conversation").await?;
    state.config.value.agents[0].runtime = "replacement".to_owned();
    let database =
        rusqlite::Connection::open(state.config.value.data_dir.join("carillon.sqlite3"))?;
    database.execute_batch(
        "CREATE TRIGGER reject_notice BEFORE INSERT ON deliveries
         BEGIN SELECT RAISE(ABORT, 'notice fault'); END;",
    )?;
    let failure = accept(&state, "later", "conversation")
        .await
        .err()
        .context("accepted a stranded turn without its notice")?;
    assert!(failure.to_string().contains("notice fault"), "{failure:#}");
    assert!(state.core.recent_turns(Some("later"), 10)?.is_empty());
    database.execute_batch("DROP TRIGGER reject_notice")?;
    let receipt = accept(&state, "later", "conversation").await?;
    assert!(!receipt.duplicate);
    assert_eq!(receipt.state, TurnState::Interrupted);
    assert_eq!(state.core.recent_turns(Some("later"), 10)?.len(), 1);
    assert_eq!(state.core.delivery_counts()?.pending, 1);
    Ok(())
}

#[tokio::test]
async fn stranded_conversation_accepts_http_input_without_its_former_runtime() -> Result<()> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let original = accept(&state, "original", "pinned").await?;
    let mut replacement = state.config.value.providers[1].clone();
    replacement.id = "replacement".to_owned();
    state.config.value.providers.push(replacement);
    state.config.value.agents[0].runtime = "replacement".to_owned();
    state.config.value.providers[1].enabled = false;
    assert_eq!(configured(&state).core.recover_interrupted()?, 1);
    for id in ["disabled", "removed"] {
        let router = Router::new()
            .route("/v1/events", post(accept_event))
            .with_state(configured(&state));
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/events")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .header(http::header::AUTHORIZATION, "Bearer test-only")
                    .header(PROVIDER_HEADER, "test-interface")
                    .body(Body::from(serde_json::to_vec(&ExternalEvent {
                        event_id: id.to_owned(),
                        agent_id: "test-agent".to_owned(),
                        user_id: "test-user".to_owned(),
                        audience_id: "pinned".to_owned(),
                        delivery_target_id: "target".to_owned(),
                        feedback_target_id: "feedback".to_owned(),
                        content: "new input".to_owned(),
                        native: Value::Null,
                        attachments: None,
                    })?))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let receipt: EventReceipt =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await?)?;
        assert!(!receipt.duplicate);
        assert_eq!(receipt.state, TurnState::Interrupted);
        let notice = state
            .core
            .claim_turn_delivery("test-interface", "instance", &receipt.turn_id)?
            .context("missing HTTP cutover notice")?;
        assert_eq!(notice.turn_state, TurnState::Interrupted);
        assert_eq!(
            notice.content,
            "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation."
        );
        accept(&state, &format!("unrelated-{id}"), "unrelated").await?;
        assert!(accept(&state, "original", "pinned").await?.duplicate);
        assert_eq!(
            state
                .core
                .database
                .inspect_turn(&original.turn_id)?
                .context("missing pinned turn")?
                .state,
            TurnState::Interrupted
        );
        state
            .config
            .value
            .providers
            .retain(|provider| provider.id != "test-runtime");
    }
    Ok(())
}
