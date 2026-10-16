use super::*;

#[tokio::test]
async fn unknown_agents_are_refused_before_attachment_capture() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    with_limits(
        &mut state,
        AttachmentLimits {
            concurrent_captures: 1,
            free_space_mib: 0,
            ..AttachmentLimits::default()
        },
    )?;
    let staged = stage(&state, "unknown", &[("a", b"must not be copied")])?;
    let mut unknown = event("unknown", "", staged);
    unknown.agent_id = "unknown-agent".to_owned();
    let refusal = submit(&state, unknown)
        .await
        .err()
        .context("unknown agent was accepted")?;
    assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
    assert_eq!(refusal.message, "unknown agent");
    assert!(matches!(
        status(&state, "unknown").await?,
        EventStatus::Unknown
    ));
    assert!(state.core.recent_turns(Some("unknown"), 10)?.is_empty());
    assert!(!state.config.value.data_dir.join("attachments").exists());

    let staged = stage(&state, "valid", &[("a", b"valid upload")])?;
    let receipt = submit(&state, event("valid", "", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert_eq!(receipt.state, TurnState::Queued);
    assert_eq!(captures(&state)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn stranded_attachments_keep_the_notice_and_receipt_without_capturing() -> Result<()> {
    for removed in [false, true] {
        let directory = tempdir()?;
        let mut state = installation(directory.path())?;
        let original = accept(&state, "original", "files").await?;
        if removed {
            state.config.value.agents.clear();
        } else {
            state.config.value.agents[0].runtime = "replacement".to_owned();
        }
        state.config.value.providers.remove(1);
        let staged = stage(&state, "stranded", &[("a", b"must not be copied")])?;
        let attached = event("attached", "", staged);
        let receipt = submit(&state, attached.clone())
            .await
            .map_err(|error| anyhow!(error.message))?;
        assert!(!receipt.duplicate);
        assert_eq!(receipt.state, TurnState::Interrupted);
        assert!(!state.config.value.data_dir.join("attachments").exists());
        let inspection = state.core.inspect_turn(&receipt.turn_id)?.context("turn")?;
        assert!(inspection.attachments.is_empty());
        assert_eq!(inspection.providers[1]["id"], "test-runtime");
        let notice = state
            .core
            .claim_turn_delivery("test-interface", "instance", &receipt.turn_id)?
            .context("missing stranded notice")?;
        assert_eq!(notice.turn_state, TurnState::Interrupted);
        assert_eq!(notice.target_id, attached.delivery_target_id);
        assert_eq!(notice.feedback_target_id, attached.feedback_target_id);
        assert_eq!(
            notice.content,
            "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation."
        );
        let text = accept_for(&state, "test-agent", "text", "files").await?;
        let text_notice = state
            .core
            .claim_turn_delivery("test-interface", "instance", &text.turn_id)?
            .context("missing text notice")?;
        assert_eq!(text_notice.content, notice.content);

        fs::remove_dir_all(staging(&state))?;
        for (event_id, turn_id) in [
            ("original", original.turn_id),
            ("attached", receipt.turn_id),
        ] {
            let duplicate = submit(
                &state,
                ExternalEvent {
                    event_id: event_id.to_owned(),
                    ..attached.clone()
                },
            )
            .await
            .map_err(|error| anyhow!(error.message))?;
            assert!(duplicate.duplicate);
            assert_eq!(duplicate.turn_id, turn_id);
        }
        assert!(!state.config.value.data_dir.join("attachments").exists());
        assert_eq!(state.core.delivery_counts()?.pending, 0);
        assert_eq!(state.core.delivery_counts()?.active, 2);
    }
    Ok(())
}

#[tokio::test]
async fn an_altered_capture_interrupts_its_turn_without_running_it() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "batch", &[("a", b"original")])?;
    let receipt = submit(&state, event("altered", "read it", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let inspection = state.core.inspect_turn(&receipt.turn_id)?.context("turn")?;
    let file = state
        .config
        .value
        .data_dir
        .join("attachments")
        .join(&inspection.attachments[0].path);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640))?;
    fs::write(&file, b"tampered")?;
    let step = run_next_turn(&state, "test-runtime", async |_, _, _| {
        Err(TurnFailure::Uncertain(anyhow!(
            "the runtime must not be called"
        )))
    })
    .await?;
    assert_eq!(step, WorkerStep::Advanced);
    let interrupted = state.core.inspect_turn(&receipt.turn_id)?.context("turn")?;
    assert_eq!(interrupted.state, TurnState::Interrupted);
    assert!(
        interrupted
            .error
            .as_deref()
            .is_some_and(|error| error.contains("changed after it was accepted")),
        "{:?}",
        interrupted.error
    );
    let notice = state
        .core
        .claim_delivery("test-interface", "instance")?
        .context("no interruption notice")?;
    assert!(
        notice.content.contains("changed or removed after"),
        "{}",
        notice.content
    );
    Ok(())
}

#[tokio::test]
async fn runtime_cutover_precedes_attachment_validation() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let staged = stage(&state, "batch", &[("a", b"original")])?;
    let receipt = submit(&state, event("stranded", "read it", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let inspection = state.core.inspect_turn(&receipt.turn_id)?.context("turn")?;
    fs::remove_file(
        state
            .config
            .value
            .data_dir
            .join("attachments")
            .join(&inspection.attachments[0].path),
    )?;
    state.config.value.agents[0].runtime = "replacement".to_owned();
    state.config.value.providers.remove(1);
    assert_eq!(configured(&state).core.recover_interrupted()?, 1);
    let notice = state
        .core
        .claim_turn_delivery("test-interface", "instance", &receipt.turn_id)?
        .context("missing runtime cutover notice")?;
    assert_eq!(
        notice.content,
        "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation."
    );
    Ok(())
}
