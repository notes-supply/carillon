use super::*;

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
