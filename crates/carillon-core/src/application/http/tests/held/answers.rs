//! An answer's files: core copies each before it records the answer, and
//! the answer's delivery names the copies however often it is claimed.

use std::{fs, os::unix::fs::PermissionsExt as _, path::Path};

use carillon_installation::config::{AttachmentLimits, MIB};
use sha2::{Digest as _, Sha256};
use tempfile::tempdir;

use super::super::{
    configured,
    durable::{DurableRuntime, RUNTIME, dispatch, durable, settle},
    live::{ok, submit},
};
use crate::{WorkerStep, application::http::*, attachments};

/// The one delivery waiting for the interface, claimed by `instance`.
fn claim(core: &Core, instance: &str) -> Result<Delivery> {
    core.claim_delivery("test-interface", instance)?
        .context("no delivery is waiting")
}

/// What an interface reads of each file a delivery names: its name, where
/// the agent had it, and the bytes of core's copy, checked against the
/// digest the delivery gives.
fn sent(store: &Path, delivery: &Delivery) -> Result<Vec<(String, String, Vec<u8>)>> {
    delivery
        .files
        .iter()
        .map(|file| {
            let bytes = fs::read(store.join(&file.path))?;
            assert_eq!(file.size, bytes.len() as u64);
            assert_eq!(file.sha256, hex::encode(Sha256::digest(&bytes)));
            Ok((file.name.clone(), file.source.clone(), bytes))
        })
        .collect()
}

fn source(path: &Path) -> String {
    path.display().to_string()
}

#[tokio::test]
async fn an_answers_files_are_kept_before_it_is_recorded_and_sent_from_the_copies() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (fixture, state) = durable(directory.path(), &runtime)?;
    let made = directory.path().join("made");
    fs::create_dir(&made)?;
    let (chart, table) = (made.join("chart.png"), made.join("data set.csv"));
    fs::write(&chart, b"\x89PNG chart")?;
    fs::write(&table, b"a,b\n1,2\n")?;
    let receipt = ok(submit(&state, "chart it", "conversation").await)?;
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    let answer = json!({"outcome": "answered", "output": "Here it is.", "files": [chart, table]});
    for _ in 0..2 {
        assert_eq!(
            settle(&state, RUNTIME, &receipt.turn_id, &answer).await?,
            StatusCode::NO_CONTENT
        );
    }
    let store = fixture.config.value.data_dir.join("attachments");
    assert_eq!(
        fs::read_dir(&store)?.count(),
        1,
        "the same outcome copies once"
    );
    fs::write(&chart, b"redrawn")?;
    fs::remove_file(&table)?;
    let claimed = claim(&state.core, "instance")?;
    assert_eq!(claimed.content, "Here it is.");
    assert_eq!(
        sent(&store, &claimed)?,
        [
            (
                "chart.png".to_owned(),
                source(&chart),
                b"\x89PNG chart".to_vec()
            ),
            (
                "data set.csv".to_owned(),
                source(&table),
                b"a,b\n1,2\n".to_vec()
            ),
        ]
    );
    for file in &claimed.files {
        let permissions = fs::metadata(store.join(&file.path))?.permissions();
        assert_eq!(
            permissions.mode() & 0o777,
            0o440,
            "kept copies are read-only"
        );
    }
    assert_eq!(claim(&state.core, "instance")?.files, claimed.files);
    let inspected = state
        .core
        .inspect_turn(&receipt.turn_id)?
        .context("no turn")?;
    assert_eq!(inspected.output.as_deref(), Some("Here it is."));
    assert_eq!(inspected.answer_files, claimed.files);
    assert_eq!(state.core.database.captured_bytes()?, 18);
    Ok(())
}

#[tokio::test]
async fn files_core_cannot_keep_are_named_in_the_answer_and_the_rest_sent() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (mut fixture, _) = durable(directory.path(), &runtime)?;
    fixture.config.value.core.attachments = AttachmentLimits {
        max_files: 4,
        max_file_mib: 1,
        free_space_mib: 0,
        ..AttachmentLimits::default()
    };
    let state = configured(&fixture);
    let made = directory.path().join("made");
    fs::create_dir(&made)?;
    let kept = made.join("notes.txt");
    let (gone, big, extra) = (made.join("gone.png"), made.join("big.bin"), made.join("x"));
    fs::write(&kept, b"kept")?;
    fs::write(&big, vec![0; usize::try_from(MIB)? + 1])?;
    fs::write(&extra, b"one too many")?;
    let receipt = ok(submit(&state, "send them", "conversation").await)?;
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    let answer = json!({
        "outcome": "answered",
        "output": "Here they are.",
        "files": [kept, gone, made, big, extra],
    });
    assert_eq!(
        settle(&state, RUNTIME, &receipt.turn_id, &answer).await?,
        StatusCode::NO_CONTENT
    );
    let claimed = claim(&state.core, "instance")?;
    let made = source(&made);
    assert_eq!(
        claimed.content,
        format!(
            "Here they are.\n\n\
             Could not attach {made}/gone.png: there is no such file.\n\
             Could not attach {made}: it is not a regular file.\n\
             Could not attach {made}/big.bin: it has 2 MiB, over the 1 MiB Carillon keeps of one file.\n\
             Could not attach {made}/x: an answer keeps at most 4 files."
        )
    );
    let store = fixture.config.value.data_dir.join("attachments");
    assert_eq!(
        sent(&store, &claimed)?,
        [("notes.txt".to_owned(), source(&kept), b"kept".to_vec())]
    );
    Ok(())
}

#[tokio::test]
async fn an_answer_whose_files_core_cannot_keep_is_recorded_with_its_text() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (fixture, state) = durable(directory.path(), &runtime)?;
    let gone = directory.path().join("gone.png");
    let receipt = ok(submit(&state, "send it", "conversation").await)?;
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    let answer = json!({"outcome": "answered", "output": "Sent.", "files": [gone]});
    assert_eq!(
        settle(&state, RUNTIME, &receipt.turn_id, &answer).await?,
        StatusCode::NO_CONTENT
    );
    let claimed = claim(&state.core, "instance")?;
    assert!(claimed.files.is_empty());
    assert_eq!(
        claimed.content,
        format!(
            "Sent.\n\nCould not attach {}: there is no such file.",
            source(&gone)
        )
    );
    assert!(!fixture.config.value.data_dir.join("attachments").exists());
    Ok(())
}

#[tokio::test]
async fn kept_files_outlast_a_stop_while_copying_and_a_restart_before_delivery() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (fixture, state) = durable(directory.path(), &runtime)?;
    let chart = directory.path().join("chart.png");
    fs::write(&chart, b"chart")?;
    let receipt = ok(submit(&state, "chart it", "conversation").await)?;
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    let answer = json!({"outcome": "answered", "output": "Here.", "files": [chart]});
    state.core.stop_capturing();
    assert_eq!(
        settle(&state, RUNTIME, &receipt.turn_id, &answer).await?,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let store = fixture.config.value.data_dir.join("attachments");
    assert_eq!(fs::read_dir(&store)?.count(), 0);
    let restart = || -> Result<AppState> {
        let restarted = Core::open(core_options(&fixture.config))?;
        restarted.recover_interrupted()?;
        assert_eq!(
            restarted.reconcile_attachments()?,
            attachments::Reconciled::default()
        );
        Ok(AppState {
            core: restarted,
            ..configured(&fixture)
        })
    };
    let state = restart()?;
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    assert_eq!(
        settle(&state, RUNTIME, &receipt.turn_id, &answer).await?,
        StatusCode::NO_CONTENT
    );
    fs::remove_file(&chart)?;
    let state = restart()?;
    let claimed = claim(&state.core, "instance")?;
    assert_eq!(
        sent(&store, &claimed)?,
        [("chart.png".to_owned(), source(&chart), b"chart".to_vec())]
    );
    attachments::validate_store(
        &fixture.config.value.data_dir,
        &state.core.database.captured_files()?,
    )?;
    Ok(())
}
