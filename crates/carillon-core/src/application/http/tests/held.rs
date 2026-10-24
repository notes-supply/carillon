//! What a durable runtime tells core of the turns it holds, the turns it
//! opens, and the controls core relays to it, each authenticated by the
//! caller's own credential.

use axum::body::Bytes;
use carillon_protocol::turns::Silence;
use tempfile::tempdir;

use super::{
    configured,
    durable::{
        DurableRuntime, RUNTIME, add_runtime, delivered, dispatch, durable, settle, state_of,
    },
    live::{lock, ok, submit},
};
use crate::application::http::*;

#[tokio::test]
async fn only_the_durable_runtime_holding_a_turn_settles_it() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (mut fixture, _) = durable(directory.path(), &runtime)?;
    add_runtime(&mut fixture, "other-runtime", "other-only", true)?;
    add_runtime(&mut fixture, "plain-runtime", "plain-only", false)?;
    let state = configured(&fixture);
    let held = ok(submit(&state, "one", "conversation").await)?;
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    let answer = json!({"outcome": "answered", "output": "Done."});
    for (caller, wanted) in [
        (("test-interface", "test-only"), StatusCode::FORBIDDEN),
        (("test-runtime", "wrong"), StatusCode::FORBIDDEN),
        (("missing-runtime", "test-only"), StatusCode::FORBIDDEN),
        (("plain-runtime", "plain-only"), StatusCode::FORBIDDEN),
        (("other-runtime", "other-only"), StatusCode::NOT_FOUND),
    ] {
        assert_eq!(
            settle(&state, caller, &held.turn_id, &answer).await?,
            wanted,
            "{caller:?}"
        );
    }
    assert_eq!(state_of(&state, &held.turn_id)?, TurnState::Running);
    // Disabled, it still settles what it accepted before.
    fixture.config.value.providers[1].enabled = false;
    let disabled = configured(&fixture);
    assert_eq!(
        settle(&disabled, RUNTIME, &held.turn_id, &answer).await?,
        StatusCode::NO_CONTENT
    );
    assert_eq!(state_of(&state, &held.turn_id)?, TurnState::Completed);
    Ok(())
}

#[tokio::test]
async fn each_outcome_settles_its_turn_and_delivery_once() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (_fixture, state) = durable(directory.path(), &runtime)?;
    let report = super::report_value(2);
    let outcomes = [
        json!({"outcome": "answered", "output": "Done.", "activity": report}),
        json!({"outcome": "failed", "explanation": "The model refused.", "activity": report}),
        json!({"outcome": "failed"}),
        json!({"outcome": "steered"}),
        json!({"outcome": "stopped"}),
    ];
    let mut turns = Vec::new();
    for (index, outcome) in outcomes.iter().enumerate() {
        let receipt = ok(submit(&state, &format!("turn {index}"), "conversation").await)?;
        assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
        for _ in 0..2 {
            assert_eq!(
                settle(&state, RUNTIME, &receipt.turn_id, outcome).await?,
                StatusCode::NO_CONTENT,
                "{outcome}"
            );
        }
        turns.push(receipt.turn_id);
    }
    let deliveries = delivered(&state)?;
    let seen = deliveries
        .iter()
        .map(|delivery| (delivery.turn_state, delivery.silence))
        .collect::<Vec<_>>();
    assert_eq!(
        seen,
        [
            (TurnState::Completed, None),
            (TurnState::Interrupted, None),
            (TurnState::Interrupted, None),
            (TurnState::Completed, Some(Silence::Steered)),
            (TurnState::Interrupted, Some(Silence::Stopped)),
        ]
    );
    assert_eq!(deliveries[0].content, "Done.");
    assert!(
        deliveries[1]
            .content
            .starts_with("That turn was interrupted: The model refused.\n")
    );
    assert!(
        deliveries[2]
            .content
            .starts_with("That turn was interrupted. ")
    );
    assert!(deliveries[3].content.is_empty() && deliveries[4].content.is_empty());
    let inspected = state.core.inspect_turn(&turns[1])?.context("no turn")?;
    assert_eq!(
        inspected.error.as_deref(),
        Some("runtime reported failure: The model refused.")
    );
    let page = state.core.jobs_at_work("test-interface", None)?;
    assert!(page.jobs.is_empty());
    let named = state.core.named_jobs("test-interface", &turns[..1])?;
    assert_eq!(named[0].1.turns[0].activity, Some(super::report(2)?));
    // Another kind of outcome for a settled turn is refused.
    let stopped = json!({"outcome": "stopped"});
    for (turn, outcome) in [(&turns[0], &stopped), (&turns[4], &outcomes[0])] {
        assert_eq!(
            settle(&state, RUNTIME, turn, outcome).await?,
            StatusCode::CONFLICT
        );
    }
    for invalid in [
        json!({"outcome": "answered", "output": "  "}),
        json!({"outcome": "answered"}),
        json!({"outcome": "completed"}),
        json!({"outcome": "stopped", "output": "Done."}),
    ] {
        assert_eq!(
            settle(&state, RUNTIME, &turns[0], &invalid).await?,
            StatusCode::BAD_REQUEST,
            "{invalid}"
        );
    }
    assert_eq!(
        settle(&state, RUNTIME, "no-such-turn", &stopped).await?,
        StatusCode::NOT_FOUND
    );
    assert!(delivered(&state)?.is_empty());
    Ok(())
}

async fn open_turn(
    state: &AppState,
    (runtime, token): (&str, &str),
    opening: &Value,
) -> Result<Result<EventReceipt, StatusCode>> {
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, runtime.parse()?);
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {token}").parse()?,
    );
    let body = Bytes::from(serde_json::to_vec(opening)?);
    Ok(runtime::open(State(state.clone()), headers, body)
        .await
        .map(|Json(receipt)| receipt)
        .map_err(|refusal| refusal.status))
}

fn opening(audience: &str, key: &str) -> Value {
    json!({
        "interface_id": "test-interface",
        "agent_id": "test-agent",
        "audience_id": audience,
        "key": key,
        "label": "The nightly summary.",
    })
}

#[tokio::test]
async fn a_runtime_opens_one_turn_per_key_in_a_conversation_of_its_own() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (_fixture, state) = durable(directory.path(), &runtime)?;
    let first = ok(submit(&state, "hello", "conversation").await)?;
    let opened = open_turn(&state, RUNTIME, &opening("conversation", "nightly"))
        .await?
        .map_err(|status| anyhow!("refused with {status}"))?;
    assert_eq!(opened.event_id, "runtime:test-runtime:nightly");
    assert_eq!(
        (opened.state, opened.duplicate),
        (TurnState::Running, false)
    );
    let again = open_turn(&state, RUNTIME, &opening("conversation", "nightly"))
        .await?
        .map_err(|status| anyhow!("refused with {status}"))?;
    assert_eq!(
        (again.turn_id.as_str(), again.duplicate),
        (opened.turn_id.as_str(), true)
    );
    let inspected = state
        .core
        .inspect_turn(&opened.turn_id)?
        .context("no turn")?;
    assert_eq!(
        (inspected.input.as_str(), inspected.user_id.as_str()),
        ("The nightly summary.", "")
    );
    // It is a job of its own at work, with no message to show feedback on.
    let page = state.core.jobs_at_work("test-interface", None)?;
    let jobs = page
        .jobs
        .iter()
        .map(|(runtime, job)| {
            (
                runtime.as_str(),
                job.origin.turn_id.as_str(),
                job.feedback_target_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        jobs,
        [
            (
                "test-runtime",
                first.turn_id.as_str(),
                Some("test-feedback".to_owned())
            ),
            ("test-runtime", opened.turn_id.as_str(), None),
        ]
    );
    // Opened, it is settled as any turn the runtime holds.
    let answer = json!({"outcome": "answered", "output": "Nightly summary."});
    assert_eq!(
        settle(&state, RUNTIME, &opened.turn_id, &answer).await?,
        StatusCode::NO_CONTENT
    );
    let delivery = delivered(&state)?;
    assert_eq!(
        (
            delivery[0].target_id.as_str(),
            delivery[0].feedback_target_id.as_str()
        ),
        ("test-destination", "")
    );
    Ok(())
}

#[tokio::test]
async fn a_runtime_opens_no_turn_in_a_conversation_that_is_not_its_own() -> Result<()> {
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (mut fixture, _) = durable(directory.path(), &runtime)?;
    add_runtime(&mut fixture, "other-runtime", "other-only", true)?;
    let state = configured(&fixture);
    ok(submit(&state, "hello", "conversation").await)?;
    for (caller, body, wanted) in [
        (RUNTIME, opening("unknown", "k"), StatusCode::CONFLICT),
        (
            ("other-runtime", "other-only"),
            opening("conversation", "k"),
            StatusCode::CONFLICT,
        ),
        (
            ("test-runtime", "wrong"),
            opening("conversation", "k"),
            StatusCode::FORBIDDEN,
        ),
        (
            RUNTIME,
            json!({"interface_id": "test-interface"}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            open_turn(&state, caller, &body).await?.err(),
            Some(wanted),
            "{body}"
        );
    }
    // An agent that moved to another runtime at a restart is not opened for
    // there, nor here.
    fixture.config.value.agents[0].runtime = "other-runtime".to_owned();
    let moved = configured(&fixture);
    for caller in [RUNTIME, ("other-runtime", "other-only")] {
        assert_eq!(
            open_turn(&moved, caller, &opening("conversation", "after"))
                .await?
                .err(),
            Some(StatusCode::CONFLICT)
        );
    }
    fixture.config.value.agents[0].runtime = "test-runtime".to_owned();
    fixture.config.value.providers[1].enabled = false;
    assert_eq!(
        open_turn(
            &configured(&fixture),
            RUNTIME,
            &opening("conversation", "off")
        )
        .await?
        .err(),
        Some(StatusCode::FORBIDDEN)
    );
    assert_eq!(state.core.turn_counts()?.active, 0);
    Ok(())
}

async fn control(state: &AppState, event_id: &str, action: &str) -> Result<(StatusCode, Value)> {
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, "test-interface".parse()?);
    headers.insert(http::header::AUTHORIZATION, "Bearer test-only".parse()?);
    let action = serde_json::from_value(json!({ "action": action }))?;
    let response = match controls::control(
        State(state.clone()),
        headers,
        AxumPath(event_id.to_owned()),
        Json(action),
    )
    .await
    {
        Ok(response) => response,
        Err(refusal) => return Ok((refusal.status, json!({ "error": refusal.message }))),
    };
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 16).await?;
    Ok((
        status,
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body)?
        },
    ))
}

#[tokio::test]
async fn controls_reach_the_runtime_holding_a_running_turn_and_its_answer_comes_back() -> Result<()>
{
    let directory = tempdir()?;
    let runtime = DurableRuntime::start().await?;
    let (mut fixture, state) = durable(directory.path(), &runtime)?;
    let held = ok(submit(&state, "one", "conversation").await)?;
    assert_eq!(
        control(&state, "unknown", "stop").await?.0,
        StatusCode::NOT_FOUND
    );
    // Queued in core, no runtime holds it yet.
    assert_eq!(
        control(&state, "one", "stop").await?.0,
        StatusCode::CONFLICT
    );
    assert_eq!(dispatch(&state).await?, WorkerStep::Advanced);
    assert_eq!(
        control(&state, "one", "steer").await?,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    *lock(&runtime.control) = (
        StatusCode::CONFLICT,
        json!({"error": "That message is already running."}),
    );
    assert_eq!(
        control(&state, "one", "interrupt").await?,
        (
            StatusCode::CONFLICT,
            json!({"error": "That message is already running."})
        )
    );
    assert_eq!(
        runtime.controlled(),
        [
            (
                held.turn_id.clone(),
                json!({"interface_id": "test-interface", "action": "steer"})
            ),
            (
                held.turn_id.clone(),
                json!({"interface_id": "test-interface", "action": "interrupt"})
            ),
        ]
    );
    let stopped = json!({"outcome": "stopped"});
    assert_eq!(
        settle(&state, RUNTIME, &held.turn_id, &stopped).await?,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        control(&state, "one", "stop").await?.0,
        StatusCode::CONFLICT
    );
    // A runtime that is not durable takes no controls.
    fixture.config.value.providers[1].durable = false;
    let plain = configured(&fixture);
    let running = ok(submit(&plain, "three", "elsewhere").await)?;
    plain
        .core
        .database
        .claim("test-runtime", Lane::Messages, &CancellationToken::new())?
        .context("nothing to claim")?;
    assert_eq!(state_of(&plain, &running.turn_id)?, TurnState::Running);
    assert_eq!(
        control(&plain, "three", "stop").await?.0,
        StatusCode::CONFLICT
    );
    assert_eq!(runtime.controlled().len(), 2);
    Ok(())
}
