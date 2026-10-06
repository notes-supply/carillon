//! Events with attachments through core's authenticated HTTP admission, its
//! capture workers, real SQLite and real files.
use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt as _,
    sync::{Arc, atomic::AtomicUsize},
};

use anyhow::bail;

use carillon_installation::config::AttachmentLimits;
use carillon_protocol::model::{StagedAttachments, StagedFile, TurnAttachment, UnansweredTurn};

use super::*;

/// Reopen core with `limits`, as a restart with that configuration would.
fn with_limits(state: &mut AppState, limits: AttachmentLimits) -> Result<()> {
    state.config.value.core.attachments = limits;
    state.core = Core::open(core_options(&state.config))?;
    Ok(())
}

fn staging(state: &AppState) -> std::path::PathBuf {
    state
        .config
        .value
        .data_dir
        .join("components/test-interface/incoming")
}

fn stage(state: &AppState, batch: &str, files: &[(&str, &[u8])]) -> Result<StagedAttachments> {
    let directory = staging(state).join(batch);
    fs::create_dir_all(&directory)?;
    for (ordinal, (_, bytes)) in files.iter().enumerate() {
        fs::write(directory.join(ordinal.to_string()), bytes)?;
    }
    Ok(StagedAttachments {
        batch: batch.to_owned(),
        files: files
            .iter()
            .map(|(name, bytes)| StagedFile {
                name: (*name).to_owned(),
                size: bytes.len() as u64,
                content_type: None,
            })
            .collect(),
    })
}

fn event(id: &str, content: &str, attachments: StagedAttachments) -> ExternalEvent {
    ExternalEvent {
        event_id: id.to_owned(),
        agent_id: "test-agent".to_owned(),
        user_id: "test-user".to_owned(),
        audience_id: "files".to_owned(),
        delivery_target_id: "test-destination".to_owned(),
        feedback_target_id: "test-feedback".to_owned(),
        content: content.to_owned(),
        native: serde_json::Value::Null,
        attachments: Some(attachments),
    }
}

fn interface_headers(state: &AppState) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, "test-interface".parse()?);
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", state.config.credential("test-interface")?).parse()?,
    );
    Ok(headers)
}

async fn submit(state: &AppState, event: ExternalEvent) -> Result<EventReceipt, ApiError> {
    let headers = interface_headers(state).map_err(ApiError::from)?;
    Ok(accept_event(State(state.clone()), headers, Json(event))
        .await?
        .0)
}

async fn status(state: &AppState, event_id: &str) -> Result<EventStatus> {
    Ok(event_status(
        State(state.clone()),
        interface_headers(state)?,
        AxumPath(event_id.to_owned()),
    )
    .await
    .map_err(|error| anyhow!(error.message))?
    .0)
}

fn captures(state: &AppState) -> Result<Vec<String>> {
    let Ok(entries) = fs::read_dir(state.config.value.data_dir.join("attachments")) else {
        return Ok(Vec::new());
    };
    let mut names = entries
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

/// Wait, without timers, until core reports the event's capture running:
/// a paused test clock does not advance while a blocking copy runs.
async fn wait_for_capture(state: &AppState, event_id: &str) -> Result<()> {
    for _ in 0..100_000 {
        if matches!(status(state, event_id).await?, EventStatus::Capturing) {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
    bail!("the capture of {event_id} never started")
}

#[tokio::test]
async fn files_are_captured_before_acceptance_and_dispatched_by_relative_path() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    assert!(matches!(
        status(&state, "files").await?,
        EventStatus::Unknown
    ));
    let staged = stage(
        &state,
        "batch",
        &[("report.pdf", b"%PDF-1.7 bytes"), ("notes\n.txt", b"plain")],
    )?;
    // Attachments alone are a message.
    let receipt = submit(&state, event("files", "", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(!receipt.duplicate);
    let EventStatus::Accepted(found) = status(&state, "files").await? else {
        bail!("accepted event has no receipt");
    };
    assert_eq!(found.turn_id, receipt.turn_id);
    let inspection = state
        .core
        .inspect_turn(&receipt.turn_id)?
        .context("missing turn")?;
    assert_eq!(inspection.attachments.len(), 2);
    assert_eq!(inspection.attachments[1].name, "notes\n.txt");
    // The interface removes its staging once it holds the receipt; the
    // accepted files no longer depend on it.
    fs::remove_dir_all(staging(&state))?;
    let root = state.config.value.data_dir.join("attachments");
    let dispatched = Arc::new(std::sync::Mutex::new(Vec::<TurnAttachment>::new()));
    let seen = dispatched.clone();
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(std::path::Path::new(&request.attachment_root), root);
        assert!(request.input.is_empty());
        for attachment in &request.attachments {
            assert!(!attachment.path.starts_with('/'));
            let bytes = fs::read(root.join(&attachment.path))
                .map_err(|error| TurnFailure::Uncertain(error.into()))?;
            assert_eq!(bytes.len() as u64, attachment.size);
        }
        seen.lock()
            .map_err(|_| TurnFailure::Uncertain(anyhow!("poisoned")))?
            .clone_from(&request.attachments);
        Ok(completed(request))
    })
    .await?;
    let dispatched = dispatched.lock().map_err(|_| anyhow!("poisoned"))?.clone();
    assert_eq!(dispatched, inspection.attachments);
    assert_eq!(fs::read(root.join(&dispatched[0].path))?, b"%PDF-1.7 bytes");
    assert_eq!(
        fs::metadata(root.join(&dispatched[0].path))?
            .permissions()
            .mode()
            & 0o777,
        0o440
    );
    // Later turns of the conversation carry the root without files.
    accept(&state, "text later", "files").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(std::path::Path::new(&request.attachment_root), root);
        assert!(request.attachments.is_empty());
        Ok(completed(request))
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn duplicates_keep_the_first_accepted_files_and_concurrent_submissions_copy_once()
-> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = stage(&state, "first", &[("a.txt", b"accepted bytes")])?;
    let receipt = submit(&state, event("once", "files", first))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let accepted = captures(&state)?;
    assert_eq!(accepted.len(), 1);
    // A later submission of the same event with other staged files is the
    // same turn; nothing new is copied and the accepted bytes stay.
    let other = stage(&state, "second", &[("a.txt", b"replaced bytes")])?;
    let duplicate = submit(&state, event("once", "files", other))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn_id, receipt.turn_id);
    assert_eq!(captures(&state)?, accepted);
    let inspection = state.core.inspect_turn(&receipt.turn_id)?.context("turn")?;
    let root = state.config.value.data_dir.join("attachments");
    assert_eq!(
        fs::read(root.join(&inspection.attachments[0].path))?,
        b"accepted bytes"
    );

    // Racing submissions of one event, with different sources, become one
    // turn and one capture.
    let batches = (0..6)
        .map(|index| {
            stage(
                &state,
                &format!("race-{index}"),
                &[("race.bin", format!("source {index}").as_bytes())],
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let mut submissions = tokio::task::JoinSet::new();
    for batch in batches {
        let (state, event) = (state.clone(), event("race", "race", batch));
        submissions.spawn(async move {
            submit(&state, event)
                .await
                .map_err(|error| anyhow!(error.message))
        });
    }
    let mut turns = BTreeSet::new();
    while let Some(receipt) = submissions.join_next().await {
        turns.insert(receipt??.turn_id);
    }
    assert_eq!(turns.len(), 1);
    assert_eq!(captures(&state)?.len(), 2);
    assert_eq!(state.core.database.captures()?.len(), 2);
    let race = state
        .core
        .inspect_turn(turns.first().context("no turn")?)?
        .context("turn")?;
    let winner = fs::read(root.join(&race.attachments[0].path))?;
    assert!(String::from_utf8(winner)?.starts_with("source "));
    assert_eq!(
        *state
            .core
            .intake
            .reserved
            .lock()
            .map_err(|_| anyhow!("poisoned"))?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn limits_and_storage_budget_refuse_before_copying_and_release_reservations() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    with_limits(
        &mut state,
        AttachmentLimits {
            max_files: 2,
            max_file_mib: 1,
            max_event_mib: 1,
            storage_mib: 1,
            free_space_mib: 0,
            concurrent_captures: 1,
        },
    )?;
    let kib = |count: usize| vec![b'x'; count * 1024];
    let refusals = [
        (
            "three files",
            stage(&state, "many", &[("1", b"1"), ("2", b"2"), ("3", b"3")])?,
        ),
        (
            "large file",
            stage(&state, "large", &[("big", &kib(1025))])?,
        ),
        (
            "large message",
            stage(&state, "sum", &[("a", &kib(600)), ("b", &kib(600))])?,
        ),
    ];
    for (case, staged) in refusals {
        let refusal = submit(&state, event(case, "", staged))
            .await
            .err()
            .context("over-limit attachments accepted")?;
        assert_eq!(refusal.status, StatusCode::BAD_REQUEST, "{case}");
    }
    assert!(captures(&state)?.is_empty());
    let kept = stage(&state, "kept", &[("kept", &kib(600))])?;
    submit(&state, event("kept", "", kept))
        .await
        .map_err(|error| anyhow!(error.message))?;
    // Accepted files count against the budget; nothing is removed to make room.
    let over = stage(&state, "over", &[("over", &kib(600))])?;
    let refusal = submit(&state, event("over", "", over.clone()))
        .await
        .err()
        .context("storage budget oversold")?;
    assert_eq!(refusal.status, StatusCode::INSUFFICIENT_STORAGE);
    assert!(
        refusal.message.contains("storage is full"),
        "{}",
        refusal.message
    );
    assert_eq!(captures(&state)?.len(), 1);
    assert_eq!(
        *state
            .core
            .intake
            .reserved
            .lock()
            .map_err(|_| anyhow!("poisoned"))?,
        0
    );

    // The free-space floor refuses too, before copying.
    with_limits(
        &mut state,
        AttachmentLimits {
            storage_mib: 1024 * 1024,
            max_event_mib: 1024,
            free_space_mib: 1024 * 1024,
            ..AttachmentLimits::default()
        },
    )?;
    let refusal = submit(&state, event("over", "", over.clone()))
        .await
        .err()
        .context("free-space floor ignored")?;
    assert_eq!(refusal.status, StatusCode::INSUFFICIENT_STORAGE);
    assert!(
        refusal.message.contains("free disk space"),
        "{}",
        refusal.message
    );
    with_limits(
        &mut state,
        AttachmentLimits {
            free_space_mib: 0,
            ..AttachmentLimits::default()
        },
    )?;
    submit(&state, event("over", "", over))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert_eq!(captures(&state)?.len(), 2);
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "the held lock is what stalls the blocking copy"
)]
async fn an_interface_clearing_its_staging_never_fails_an_admitted_capture() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let files: [(&str, &[u8]); 3] = [("a", b"first"), ("b", b"second"), ("c", b"third")];
    // A restarting interface's cleanup overtakes core while it opens a
    // batch: the first file is there, the later ones are gone. Core refuses
    // the submission without admitting it, and holds and keeps nothing.
    let overtaken = stage(&state, "overtaken", &files)?;
    for ordinal in ["1", "2"] {
        fs::remove_file(staging(&state).join("overtaken").join(ordinal))?;
    }
    let refusal = submit(&state, event("overtaken", "", overtaken))
        .await
        .err()
        .context("a batch missing files was admitted")?;
    assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
    assert!(matches!(
        status(&state, "overtaken").await?,
        EventStatus::Unknown
    ));
    assert!(captures(&state)?.is_empty());
    assert_eq!(state.core.intake.workers.available_permits(), 2);
    assert_eq!(
        *state
            .core
            .intake
            .reserved
            .lock()
            .map_err(|_| anyhow!("poisoned"))?,
        0
    );

    // Once core reports a capture, it reads the files it opened, whatever
    // becomes of the staging.
    let staged = stage(&state, "admitted", &files)?;
    let held = state
        .core
        .intake
        .signals
        .stall
        .lock()
        .map_err(|_| anyhow!("poisoned"))?;
    let submission = {
        let (state, event) = (state.clone(), event("admitted", "", staged));
        tokio::spawn(async move { submit(&state, event).await })
    };
    wait_for_capture(&state, "admitted").await?;
    fs::remove_dir_all(staging(&state))?;
    drop(held);
    let receipt = submission.await?.map_err(|error| anyhow!(error.message))?;
    let inspection = state
        .core
        .inspect_turn(&receipt.turn_id)?
        .context("missing turn")?;
    let root = state.config.value.data_dir.join("attachments");
    assert_eq!(inspection.attachments.len(), files.len());
    for (attachment, (_, bytes)) in inspection.attachments.iter().zip(files) {
        assert_eq!(fs::read(root.join(&attachment.path))?, bytes);
    }
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "the held lock is what stalls the blocking copy"
)]
async fn an_abandoned_submission_keeps_its_capture_and_worker_until_the_copy_settles() -> Result<()>
{
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
    let slow = stage(&state, "slow", &[("slow.bin", &[7; 64])])?;
    let stall = state.core.intake.signals.stall.clone();
    let held = stall.lock().map_err(|_| anyhow!("poisoned"))?;
    let first = {
        let (state, event) = (state.clone(), event("slow", "", slow.clone()));
        tokio::spawn(async move { submit(&state, event).await })
    };
    wait_for_capture(&state, "slow").await?;
    // The submitting connection goes away; core keeps capturing.
    first.abort();
    assert!(first.await.is_err());
    assert!(matches!(
        status(&state, "slow").await?,
        EventStatus::Capturing
    ));
    assert_eq!(state.core.intake.workers.available_permits(), 0);
    // Other events are refused as busy before any work, not queued.
    let other = stage(&state, "other", &[("other", b"other")])?;
    let busy = submit(&state, event("other", "", other.clone()))
        .await
        .err()
        .context("capture bound exceeded")?;
    assert_eq!(busy.status, StatusCode::SERVICE_UNAVAILABLE);
    // A retry of the same event joins the running copy instead of starting one.
    let retry = {
        let (state, event) = (state.clone(), event("slow", "", slow));
        tokio::spawn(async move { submit(&state, event).await })
    };
    tokio::task::yield_now().await;
    assert!(!retry.is_finished());
    drop(held);
    let receipt = retry.await?.map_err(|error| anyhow!(error.message))?;
    assert!(!receipt.duplicate);
    assert_eq!(captures(&state)?.len(), 1);
    assert_eq!(state.core.intake.workers.available_permits(), 1);
    submit(&state, event("other", "", other))
        .await
        .map_err(|error| anyhow!(error.message))?;
    Ok(())
}

#[tokio::test(start_paused = true)]
#[expect(
    clippy::await_holding_lock,
    reason = "the held lock is what stalls the blocking copy"
)]
async fn a_capture_past_its_deadline_is_refused_only_after_the_copy_returns() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "stalled", &[("stalled.bin", &[1; 64])])?;
    let held = state
        .core
        .intake
        .signals
        .stall
        .lock()
        .map_err(|_| anyhow!("poisoned"))?;
    let submission = {
        let (state, event) = (state.clone(), event("stalled", "", staged));
        tokio::spawn(async move { submit(&state, event).await })
    };
    wait_for_capture(&state, "stalled").await?;
    // The paused clock passes the deadline, but the copy is blocked inside
    // the filesystem: its worker stays taken and nothing is decided.
    tokio::time::advance(crate::intake::CAPTURE_DEADLINE * 2).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(!submission.is_finished());
    assert_eq!(state.core.intake.workers.available_permits(), 1);
    assert!(matches!(
        status(&state, "stalled").await?,
        EventStatus::Capturing
    ));
    drop(held);
    let refusal = submission
        .await?
        .err()
        .context("cancelled capture accepted")?;
    assert_eq!(refusal.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(captures(&state)?.is_empty());
    assert!(matches!(
        status(&state, "stalled").await?,
        EventStatus::Unknown
    ));
    assert_eq!(state.core.intake.workers.available_permits(), 2);
    Ok(())
}

#[tokio::test(start_paused = true)]
#[expect(
    clippy::await_holding_lock,
    reason = "the held lock is what stalls the publishing copy"
)]
async fn a_capture_cancelled_while_publishing_makes_no_new_acceptance() -> Result<()> {
    use std::sync::atomic::Ordering;
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let signals = &state.core.intake.signals;
    // Run `event` until its copy is publishing, pass the deadline there,
    // with `meanwhile` happening, then let publication finish.
    let past_deadline_while_publishing = async |event: ExternalEvent, meanwhile| {
        signals.publishing.store(false, Ordering::SeqCst);
        let held = signals
            .publication
            .lock()
            .map_err(|_| anyhow!("poisoned"))?;
        let submission = {
            let state = state.clone();
            tokio::spawn(async move { submit(&state, event).await })
        };
        for _ in 0..100_000 {
            if signals.publishing.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        anyhow::ensure!(
            signals.publishing.load(Ordering::SeqCst),
            "the copy never reached publication"
        );
        tokio::time::advance(crate::intake::CAPTURE_DEADLINE * 2).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        anyhow::ensure!(
            !submission.is_finished(),
            "decided before the copy returned"
        );
        if let Some(text) = meanwhile {
            submit(&state, text)
                .await
                .map_err(|error| anyhow!(error.message))?;
        }
        drop(held);
        Ok(submission.await?)
    };
    let staged = stage(&state, "publishing", &[("a.bin", &[1; 64])])?;
    let refusal = past_deadline_while_publishing(event("publishing", "", staged.clone()), None)
        .await?
        .err()
        .context("a capture cancelled while publishing was accepted")?;
    assert_eq!(refusal.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(captures(&state)?.is_empty());
    assert!(matches!(
        status(&state, "publishing").await?,
        EventStatus::Unknown
    ));
    assert_eq!(state.core.intake.workers.available_permits(), 2);
    // An event accepted another way while its copy published keeps that
    // acceptance; the copy is still removed.
    let mut text = event("accepted", "words only", staged.clone());
    text.attachments = None;
    let receipt = past_deadline_while_publishing(event("accepted", "", staged.clone()), Some(text))
        .await?
        .map_err(|error| anyhow!(error.message))?;
    assert!(receipt.duplicate);
    assert!(
        state
            .core
            .inspect_turn(&receipt.turn_id)?
            .context("turn")?
            .attachments
            .is_empty()
    );
    assert!(captures(&state)?.is_empty());
    // Nothing is left behind: the event can be sent again.
    let retried = submit(&state, event("publishing", "", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(!retried.duplicate);
    assert_eq!(captures(&state)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_failed_commit_leaves_no_capture_and_the_event_can_be_retried() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "batch", &[("a", b"bytes")])?;
    let database =
        rusqlite::Connection::open(state.config.value.data_dir.join("carillon.sqlite3"))?;
    database.execute_batch(
        "CREATE TRIGGER fail_attachment BEFORE INSERT ON turn_attachments
         BEGIN SELECT RAISE(ABORT, 'injected attachment failure'); END;",
    )?;
    let failure = submit(&state, event("commit", "x", staged.clone()))
        .await
        .err()
        .context("injected failure did not fire")?;
    assert_eq!(failure.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(captures(&state)?.is_empty());
    assert!(
        state
            .core
            .database
            .event_receipt("test-interface", "commit")?
            .is_none()
    );
    database.execute_batch("DROP TRIGGER fail_attachment;")?;
    submit(&state, event("commit", "x", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert_eq!(captures(&state)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn startup_reconciliation_keeps_accepted_files_and_removes_crash_leftovers() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "batch", &[("a", b"accepted")])?;
    let receipt = submit(&state, event("kept", "", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let store = state.config.value.data_dir.join("attachments");
    let accepted = captures(&state)?;
    // Crash during a copy, and crash after publishing before the commit.
    fs::create_dir(store.join(".partial-0190a6e0-0000-7000-8000-000000000001"))?;
    let orphan = store.join("0190a6e0-0000-7000-8000-000000000002");
    fs::create_dir(&orphan)?;
    fs::write(orphan.join("0"), b"published, never committed")?;
    let reconciled = state.core.reconcile_attachments()?;
    assert_eq!((reconciled.partial, reconciled.unreferenced), (1, 1));
    assert_eq!(captures(&state)?, accepted);
    let ran = Arc::new(AtomicUsize::new(0));
    let count = ran.clone();
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(request.correlation_id, receipt.turn_id);
        Ok(completed(request))
    })
    .await?;
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
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

/// Run the next turn, answering with `output`, and return the earlier files
/// it was offered. An empty answer ends the turn unanswered, as a command
/// that consumed its message does.
async fn offered(state: &AppState, output: &str) -> Result<Vec<UnansweredTurn>> {
    offered_answering(state, serde_json::json!({ "output": output })).await
}

/// Run the next turn, answering with the fields of `answer` as a runtime's
/// response reads on the wire, and return the earlier files it was offered.
async fn offered_answering(
    state: &AppState,
    mut answer: serde_json::Value,
) -> Result<Vec<UnansweredTurn>> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    run_next_turn(state, "test-runtime", async move |_, _, request| {
        record
            .lock()
            .map_err(|_| TurnFailure::Uncertain(anyhow!("poisoned")))?
            .clone_from(&request.unanswered);
        answer["correlation_id"] = request.correlation_id.into();
        serde_json::from_value(answer).map_err(|error| TurnFailure::Uncertain(error.into()))
    })
    .await?;
    let offered = seen.lock().map_err(|_| anyhow!("poisoned"))?.clone();
    Ok(offered)
}

#[tokio::test]
async fn files_of_unanswered_turns_are_offered_again_until_a_turn_is_answered() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(
        &state,
        "command",
        &[("a.txt", b"first"), ("b.txt", b"second")],
    )?;
    let command = submit(&state, event("command", "/note", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(offered(&state, "").await?.is_empty());
    // Another conversation's unanswered files stay with it.
    let mut elsewhere = event(
        "elsewhere",
        "/note",
        stage(&state, "elsewhere", &[("c.txt", b"other")])?,
    );
    "other".clone_into(&mut elsewhere.audience_id);
    submit(&state, elsewhere)
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(offered(&state, "").await?.is_empty());
    accept(&state, "describe", "files").await?;
    let again = offered(&state, "the files say first and second").await?;
    let accepted = state
        .core
        .inspect_turn(&command.turn_id)?
        .context("turn")?
        .attachments;
    assert_eq!(
        again,
        vec![UnansweredTurn {
            correlation_id: command.turn_id,
            attachments: accepted,
        }]
    );
    // Once a turn of the conversation is answered, they are not offered again.
    accept(&state, "later", "files").await?;
    assert!(offered(&state, "another answer").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn files_an_answered_command_left_are_offered_again_until_a_turn_carries_them() -> Result<()>
{
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    // A message with a file is interrupted before it is answered.
    let staged = stage(&state, "photo", &[("photo.png", b"pixels")])?;
    let photo = submit(&state, event("photo", "look", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(offered(&state, "").await?.is_empty());
    let left = vec![UnansweredTurn {
        correlation_id: photo.turn_id.clone(),
        attachments: state
            .core
            .inspect_turn(&photo.turn_id)?
            .context("turn")?
            .attachments,
    }];
    // A command is answered after it ran as written, as T3's runtime runs
    // one, without the file it was offered.
    accept(&state, "compact", "files").await?;
    let command = serde_json::json!({
        "output": "`/compact` finished.",
        "unanswered_left": true,
    });
    assert_eq!(offered_answering(&state, command).await?, left);
    // The next message is offered the file again, and its answer carries it.
    accept(&state, "prose", "files").await?;
    assert_eq!(offered(&state, "the photo is pixels").await?, left);
    accept(&state, "later", "files").await?;
    assert!(offered(&state, "another answer").await?.is_empty());
    Ok(())
}

/// What `receipt`'s turn holds, as core offers it to a later turn.
fn held(state: &AppState, receipt: &EventReceipt) -> Result<UnansweredTurn> {
    Ok(UnansweredTurn {
        correlation_id: receipt.turn_id.clone(),
        attachments: state
            .core
            .inspect_turn(&receipt.turn_id)?
            .context("turn")?
            .attachments,
    })
}

#[tokio::test]
async fn files_a_command_left_of_its_own_are_offered_to_later_prose() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    // A command with its own file runs as written, without that file.
    let staged = stage(&state, "command", &[("notes.txt", b"plain notes")])?;
    let command = submit(&state, event("command", "/compact", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let answered = serde_json::json!({
        "output": "`/compact` finished.",
        "attachments_left": true,
    });
    assert!(offered_answering(&state, answered).await?.is_empty());
    // The next message is offered the command's file, and its answer carries it.
    accept(&state, "prose", "files").await?;
    assert_eq!(
        offered(&state, "the notes are plain").await?,
        vec![held(&state, &command)?]
    );
    accept(&state, "later", "files").await?;
    assert!(offered(&state, "another answer").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_command_that_left_earlier_and_own_files_leaves_both_for_later_prose() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    // A message with a file is interrupted before it is answered.
    let staged = stage(&state, "photo", &[("photo.png", b"pixels")])?;
    let photo = submit(&state, event("photo", "look", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(offered(&state, "").await?.is_empty());
    // A command with a file of its own leaves that file and the earlier one.
    let staged = stage(&state, "command", &[("notes.txt", b"plain notes")])?;
    let command = submit(&state, event("command", "/compact", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let answered = serde_json::json!({
        "output": "`/compact` finished.",
        "unanswered_left": true,
        "attachments_left": true,
    });
    assert_eq!(
        offered_answering(&state, answered).await?,
        vec![held(&state, &photo)?]
    );
    // The next message is offered both, oldest first.
    accept(&state, "prose", "files").await?;
    assert_eq!(
        offered(&state, "both are read").await?,
        vec![held(&state, &photo)?, held(&state, &command)?]
    );
    accept(&state, "later", "files").await?;
    assert!(offered(&state, "another answer").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn every_file_of_initial_commands_is_offered_until_an_answered_turn_carries_it() -> Result<()>
{
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    with_limits(
        &mut state,
        AttachmentLimits {
            max_files: 2,
            free_space_mib: 0,
            ..AttachmentLimits::default()
        },
    )?;
    // Three commands open the conversation with two files each: more than
    // one event's worth, and none of them answered.
    let mut commands = Vec::new();
    for name in ["first", "second", "third"] {
        let staged = stage(&state, name, &[("a", name.as_bytes()), ("b", b"b")])?;
        let receipt = submit(&state, event(name, "/note", staged))
            .await
            .map_err(|error| anyhow!(error.message))?;
        offered(&state, "").await?;
        let files = state
            .core
            .inspect_turn(&receipt.turn_id)?
            .context("turn")?
            .attachments;
        commands.push(UnansweredTurn {
            correlation_id: receipt.turn_id,
            attachments: files,
        });
    }
    let [first, second, third] = commands.as_slice() else {
        bail!("expected three commands");
    };
    let altered = state
        .config
        .value
        .data_dir
        .join("attachments")
        .join(&second.attachments[1].path);
    fs::set_permissions(&altered, fs::Permissions::from_mode(0o640))?;
    fs::write(&altered, b"c")?;
    let ask = async |id: &str, answer: &str| {
        accept(&state, id, "files").await?;
        offered(&state, answer).await
    };
    // Each answered turn carries the oldest files still waiting, one
    // event's worth at a time; an unanswered one carries nothing for good.
    assert_eq!(ask("ask-1", "an answer").await?, vec![first.clone()]);
    let intact = UnansweredTurn {
        correlation_id: second.correlation_id.clone(),
        attachments: vec![second.attachments[0].clone()],
    };
    assert_eq!(ask("ask-2", "").await?, vec![intact.clone()]);
    // A file no longer the one accepted is left out, and not offered again
    // once a turn has been answered without it.
    assert_eq!(ask("ask-3", "an answer").await?, vec![intact]);
    assert_eq!(ask("ask-4", "an answer").await?, vec![third.clone()]);
    assert!(ask("ask-5", "an answer").await?.is_empty());
    // A turn's own files are carried by the turn itself once it is answered.
    let staged = stage(&state, "own", &[("own", b"own")])?;
    submit(&state, event("own", "look", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    assert!(offered(&state, "seen").await?.is_empty());
    assert!(ask("ask-6", "an answer").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn earlier_files_that_cannot_be_read_now_are_offered_whole_once_they_can() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    // Two commands open the conversation: one with a file, one with a pair.
    let mut commands = Vec::new();
    for (name, files) in [
        ("alone", vec![("a", b"a".as_slice())]),
        ("pair", vec![("b", b"b".as_slice()), ("c", b"c".as_slice())]),
    ] {
        let receipt = submit(&state, event(name, "/note", stage(&state, name, &files)?))
            .await
            .map_err(|error| anyhow!(error.message))?;
        offered(&state, "").await?;
        let attachments = state
            .core
            .inspect_turn(&receipt.turn_id)?
            .context("turn")?
            .attachments;
        commands.push(UnansweredTurn {
            correlation_id: receipt.turn_id,
            attachments,
        });
    }
    let root = state.config.value.data_dir.join("attachments");
    let unreadable = [
        root.join(&commands[0].attachments[0].path),
        root.join(&commands[1].attachments[1].path),
    ];
    for file in &unreadable {
        fs::set_permissions(file, fs::Permissions::from_mode(0o000))?;
    }
    // A turn with a file that cannot be read now is not offered, not even
    // its readable sibling, and an answer does not retire it.
    accept(&state, "ask-1", "files").await?;
    let withheld = offered(&state, "an answer").await;
    for file in &unreadable {
        fs::set_permissions(file, fs::Permissions::from_mode(0o440))?;
    }
    assert!(withheld?.is_empty());
    // Once readable, each is offered whole, in order, and then carried.
    accept(&state, "ask-2", "files").await?;
    assert_eq!(offered(&state, "an answer").await?, commands);
    accept(&state, "ask-3", "files").await?;
    assert!(offered(&state, "an answer").await?.is_empty());
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "the held lock is what stalls the blocking copy"
)]
async fn a_conversations_later_messages_wait_for_its_capture_in_progress() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "first", &[("a", b"first")])?;
    let held = state
        .core
        .intake
        .signals
        .stall
        .lock()
        .map_err(|_| anyhow!("poisoned"))?;
    let first = {
        let (state, event) = (state.clone(), event("first", "", staged.clone()));
        tokio::spawn(async move { submit(&state, event).await })
    };
    wait_for_capture(&state, "first").await?;
    // Whichever instance of the interface sends them, later messages of the
    // conversation wait rather than overtake it: text, and files before any
    // copying starts.
    let mut later = event("later", "after the file", staged.clone());
    later.attachments = None;
    let refused = submit(&state, later.clone())
        .await
        .err()
        .context("a later message overtook a capture in progress")?;
    assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    let more = stage(&state, "more", &[("b", b"more")])?;
    let refused = submit(&state, event("more", "", more.clone()))
        .await
        .err()
        .context("later files overtook a capture in progress")?;
    assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        *state
            .core
            .intake
            .reserved
            .lock()
            .map_err(|_| anyhow!("poisoned"))?,
        5
    );
    // Other conversations are not held up.
    let mut elsewhere = later.clone();
    "elsewhere".clone_into(&mut elsewhere.event_id);
    "other".clone_into(&mut elsewhere.audience_id);
    submit(&state, elsewhere)
        .await
        .map_err(|error| anyhow!(error.message))?;
    drop(held);
    let first = first.await?.map_err(|error| anyhow!(error.message))?;
    let later = submit(&state, later)
        .await
        .map_err(|error| anyhow!(error.message))?;
    submit(&state, event("more", "", more))
        .await
        .map_err(|error| anyhow!(error.message))?;
    // The conversation's turns run in the order its messages were sent.
    let mut order = Vec::new();
    for _ in 0..4 {
        let ran = Arc::new(std::sync::Mutex::new(String::new()));
        let record = ran.clone();
        run_next_turn(&state, "test-runtime", async move |_, _, request| {
            record
                .lock()
                .map_err(|_| TurnFailure::Uncertain(anyhow!("poisoned")))?
                .clone_from(&request.correlation_id);
            Ok(completed(request))
        })
        .await?;
        order.push(ran.lock().map_err(|_| anyhow!("poisoned"))?.clone());
    }
    let position = |turn: &str| order.iter().position(|ran| ran == turn);
    assert!(
        position(&first.turn_id) < position(&later.turn_id),
        "{order:?}"
    );
    Ok(())
}

#[tokio::test]
async fn status_and_staging_belong_to_the_authenticated_interface() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let staged = stage(&state, "batch", &[("a", b"mine")])?;
    submit(&state, event("mine", "", staged.clone()))
        .await
        .map_err(|error| anyhow!(error.message))?;
    // Another interface neither sees the receipt nor reads this staging.
    foreign_turn(&mut state, directory.path()).await?;
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, "other-interface".parse()?);
    headers.insert(http::header::AUTHORIZATION, "Bearer other-only".parse()?);
    let other = event_status(
        State(state.clone()),
        headers.clone(),
        AxumPath("mine".to_owned()),
    )
    .await
    .map_err(|error| anyhow!(error.message))?
    .0;
    assert!(matches!(other, EventStatus::Unknown));
    let refusal = accept_event(
        State(configured(&state)),
        headers,
        Json(ExternalEvent {
            event_id: "borrowed".to_owned(),
            audience_id: "elsewhere".to_owned(),
            ..event("borrowed", "", staged)
        }),
    )
    .await
    .err()
    .context("read another interface's staging")?;
    assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
    // Unauthenticated status requests learn nothing.
    let anonymous = event_status(
        State(state.clone()),
        HeaderMap::new(),
        AxumPath("mine".to_owned()),
    )
    .await
    .err()
    .context("unauthenticated status")?;
    assert_eq!(anonymous.status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "the held lock is what stalls the blocking copy"
)]
async fn a_capture_that_loses_to_an_earlier_acceptance_is_removed() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "late", &[("late.bin", b"late bytes")])?;
    let held = state
        .core
        .intake
        .signals
        .stall
        .lock()
        .map_err(|_| anyhow!("poisoned"))?;
    let capturing = {
        let (state, event) = (state.clone(), event("raced", "", staged.clone()));
        tokio::spawn(async move { submit(&state, event).await })
    };
    wait_for_capture(&state, "raced").await?;
    // The same event is accepted another way while the copy is stalled.
    let won = submit(
        &state,
        ExternalEvent {
            attachments: None,
            ..event("raced", "without files", staged)
        },
    )
    .await
    .map_err(|error| anyhow!(error.message))?;
    assert!(!won.duplicate);
    drop(held);
    let joined = capturing.await?.map_err(|error| anyhow!(error.message))?;
    assert!(joined.duplicate);
    assert_eq!(joined.turn_id, won.turn_id);
    // The losing copy was removed once the database showed nothing uses it.
    assert!(captures(&state)?.is_empty());
    assert!(state.core.database.captures()?.is_empty());
    assert_eq!(
        *state
            .core
            .intake
            .reserved
            .lock()
            .map_err(|_| anyhow!("poisoned"))?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_reservations_never_oversell_the_storage_budget() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    with_limits(
        &mut state,
        AttachmentLimits {
            max_files: 1,
            max_file_mib: 1,
            max_event_mib: 1,
            storage_mib: 2,
            free_space_mib: 0,
            concurrent_captures: 8,
        },
    )?;
    let bytes = vec![7_u8; 600 * 1024];
    let mut submissions = tokio::task::JoinSet::new();
    // Separate conversations, so their captures may run together.
    for index in 0..8 {
        let staged = stage(&state, &format!("event-{index}"), &[("part", &bytes)])?;
        let mut event = event(&format!("event-{index}"), "", staged);
        event.audience_id = format!("conversation-{index}");
        let state = state.clone();
        submissions.spawn(async move { submit(&state, event).await });
    }
    let (mut accepted, mut refused) = (0, 0);
    while let Some(outcome) = submissions.join_next().await {
        match outcome? {
            Ok(_) => accepted += 1,
            Err(refusal) => {
                assert_eq!(refusal.status, StatusCode::INSUFFICIENT_STORAGE);
                refused += 1;
            }
        }
    }
    // 2 MiB holds three 600 KiB captures, however the reservations interleave.
    assert_eq!((accepted, refused), (3, 5));
    assert_eq!(state.core.database.captured_bytes()?, 3 * 600 * 1024);
    assert_eq!(captures(&state)?.len(), 3);
    assert_eq!(
        *state
            .core
            .intake
            .reserved
            .lock()
            .map_err(|_| anyhow!("poisoned"))?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn declared_media_types_may_carry_parameters_but_not_controls() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let mut staged = stage(&state, "typed", &[("notes.txt", b"plain notes")])?;
    staged.files[0].content_type = Some("text/plain; charset=utf-8".to_owned());
    let receipt = submit(&state, event("typed", "", staged.clone()))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let inspection = state.core.inspect_turn(&receipt.turn_id)?.context("turn")?;
    assert_eq!(
        inspection.attachments[0].content_type.as_deref(),
        Some("text/plain; charset=utf-8")
    );
    staged.files[0].content_type = Some("text/plain\u{7}".to_owned());
    let refusal = submit(&state, event("controlled", "", staged))
        .await
        .err()
        .context("control character accepted")?;
    assert_eq!(refusal.status, StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn an_unreadable_capture_is_not_reported_as_altered() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "batch", &[("a", b"original")])?;
    let receipt = submit(&state, event("unreadable", "read it", staged))
        .await
        .map_err(|error| anyhow!(error.message))?;
    let inspection = state.core.inspect_turn(&receipt.turn_id)?.context("turn")?;
    let file = state
        .config
        .value
        .data_dir
        .join("attachments")
        .join(&inspection.attachments[0].path);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000))?;
    let step = run_next_turn(&state, "test-runtime", async |_, _, _| {
        Err(TurnFailure::Uncertain(anyhow!(
            "the runtime must not be called"
        )))
    })
    .await;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o440))?;
    assert_eq!(step?, WorkerStep::Advanced);
    let notice = state
        .core
        .claim_delivery("test-interface", "instance")?
        .context("no interruption notice")?;
    assert!(
        notice.content.contains("could not be read"),
        "{}",
        notice.content
    );
    Ok(())
}

#[tokio::test]
#[expect(
    clippy::await_holding_lock,
    reason = "the held lock is what stalls the blocking copy"
)]
async fn shutdown_waits_for_running_captures_to_settle() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let staged = stage(&state, "settling", &[("a", b"bytes")])?;
    let held = state
        .core
        .intake
        .signals
        .stall
        .lock()
        .map_err(|_| anyhow!("poisoned"))?;
    let submission = {
        let (state, event) = (state.clone(), event("settling", "", staged));
        tokio::spawn(async move { submit(&state, event).await })
    };
    wait_for_capture(&state, "settling").await?;
    state.core.stop_capturing();
    let settled = {
        let core = state.core.clone();
        tokio::spawn(async move { core.captures_settled().await })
    };
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(!settled.is_finished());
    drop(held);
    settled.await?;
    let refusal = submission
        .await?
        .err()
        .context("a stopped capture was accepted")?;
    assert_eq!(refusal.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(captures(&state)?.is_empty());
    // A stopped core refuses new captures before any work.
    let later = stage(&state, "later", &[("b", b"bytes")])?;
    let refused = submit(&state, event("later", "", later))
        .await
        .err()
        .context("capture accepted after shutdown")?;
    assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    Ok(())
}
