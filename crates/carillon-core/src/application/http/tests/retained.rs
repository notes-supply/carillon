//! An interface whose registry entry says `delivery = "retained"`: its
//! people read answers from core's record, so every way core ends one of
//! its turns records the answer as delivered, and nothing waits for its
//! program.

use std::path::Path;

use anyhow::Context as _;
use carillon_protocol::model::DeliveryState;
use tempfile::tempdir;

use super::{
    Fixture, accept, configured,
    durable::{DurableRuntime, RUNTIME, dispatch, durable, settle},
    hand_next,
    held::{open_turn, opening},
    live::{ok, operator, submit},
};
use crate::application::http::*;

/// The installation serving `runtime`, its interface's answers delivered
/// as `mode` says.
fn delivering(
    directory: &Path,
    runtime: &DurableRuntime,
    mode: DeliveryMode,
) -> Result<(Fixture, AppState)> {
    let (mut fixture, _) = durable(directory, runtime)?;
    fixture.config.value.providers[0].delivery = mode;
    let state = configured(&fixture);
    Ok((fixture, state))
}

fn delivery(core: &Core, turn_id: &str) -> Result<Option<DeliveryState>> {
    Ok(core
        .inspect_turn(turn_id)?
        .context("no such turn")?
        .delivery_state)
}

/// Nothing of the interface's waits to be claimed or acknowledged.
fn nothing_waits(core: &Core) -> Result<()> {
    assert_eq!(core.undelivered(&["test-interface"])?, 0);
    assert!(core.claim_delivery("test-interface", "instance")?.is_none());
    let reconciled = core.reconcile_deliveries("test-interface", "next")?;
    assert!(reconciled.feedback_target_ids.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_retained_interface_with_no_program_holds_no_pause() -> Result<()> {
    for (mode, undelivered) in [(DeliveryMode::External, 1), (DeliveryMode::Retained, 0)] {
        let directory = tempdir()?;
        let runtime = DurableRuntime::start().await?;
        let (_fixture, state) = delivering(directory.path(), &runtime, mode)?;
        let held = ok(submit(&state, "one", "conversation").await)?;
        dispatch(&state).await?;
        let lease = Lease { lease_ms: 30_000 };
        let holder = ok(maintenance::begin(State(state.clone()), operator()?, Json(lease)).await)?
            .0
            .holder;
        let answer = json!({"outcome": "answered", "output": "Done."});
        assert_eq!(
            settle(&state, RUNTIME, &held.turn_id, &answer).await?,
            StatusCode::NO_CONTENT
        );
        let Json(drain) =
            ok(maintenance::renew(State(state.clone()), operator()?, AxumPath(holder)).await)?;
        assert_eq!(drain.undelivered, undelivered, "{mode:?}");
    }
    Ok(())
}

#[tokio::test]
async fn a_retained_interfaces_refused_and_opened_turns_are_delivered_as_recorded() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (_fixture, state) = delivering(directory.path(), &runtime, DeliveryMode::Retained)?;
    let refused = ok(submit(&state, "one", "conversation").await)?;
    runtime.answer([(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"error": "That agent is not set up here."}),
    )]);
    dispatch(&state).await?;
    assert_eq!(
        delivery(&state.core, &refused.turn_id)?,
        Some(DeliveryState::Delivered)
    );
    let opened = open_turn(&state, RUNTIME, &opening("conversation", "nightly"))
        .await?
        .map_err(|status| anyhow!("refused with {status}"))?;
    let report = json!({"outcome": "answered", "output": "The nightly summary."});
    assert_eq!(
        settle(&state, RUNTIME, &opened.receipt.turn_id, &report).await?,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delivery(&state.core, &opened.receipt.turn_id)?,
        Some(DeliveryState::Delivered)
    );
    nothing_waits(&state.core)
}

#[tokio::test]
async fn a_retained_interfaces_notices_for_lost_runtimes_are_delivered_as_recorded() -> Result<()> {
    let directory = tempdir()?;
    let mut fixture = super::installation(directory.path())?;
    fixture.config.value.providers[0].delivery = DeliveryMode::Retained;
    let running = accept(&fixture, "running", "conversation").await?;
    hand_next(&configured(&fixture).core, "test-runtime")
        .await?
        .context("nothing was queued")?;
    let queued = accept(&fixture, "queued", "conversation").await?;
    fixture.config.value.agents.clear();
    fixture.config.value.providers.remove(1);
    let core = configured(&fixture).core;
    assert_eq!(core.recover_interrupted()?, 2);
    let later = super::accept_for(&fixture, "test-agent", "later", "conversation").await?;
    for turn_id in [running.turn_id, queued.turn_id, later.turn_id] {
        assert_eq!(delivery(&core, &turn_id)?, Some(DeliveryState::Delivered));
    }
    nothing_waits(&core)
}
