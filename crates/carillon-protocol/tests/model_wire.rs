//! Both languages consume the same wire examples, including canonical round trips.
use anyhow::{Context as _, Result};
use carillon_protocol::{
    model::{CommandCatalog, ExternalEvent, OpenedTurn, RuntimeTurnRequest, TurnActivity},
    settings::{Page, RuntimeSettings, SettingsRequest, Subject, Value as SettingValue},
    turns::{RunningTurn, RuntimeTurnControl, TurnOpening, TurnOutcome},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

fn round_trip<T: DeserializeOwned + Serialize>(value: &Value) -> Result<()> {
    let parsed: T = serde_json::from_value(value.clone())?;
    assert_eq!(serde_json::to_value(parsed)?, *value);
    Ok(())
}

/// The wire contract for an invalid value is only that both languages refuse it.
fn refused<T: DeserializeOwned>(value: &Value) -> Result<()> {
    serde_json::from_value::<T>(value.clone())
        .err()
        .map(drop)
        .with_context(|| format!("accepted invalid {value}"))
}

#[derive(Deserialize)]
struct SettingsFixtures {
    subjects: Vec<Value>,
    invalid_subjects: Vec<Value>,
    values: Vec<Value>,
    invalid_values: Vec<Value>,
    requests: Vec<Value>,
    invalid_requests: Vec<Value>,
    settings: Vec<Value>,
    invalid_settings: Vec<Value>,
    pages: Vec<Value>,
}

#[test]
fn shared_settings_wire_fixtures() -> Result<()> {
    let fixtures: SettingsFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/settings.json"
    ))?;
    for value in &fixtures.subjects {
        round_trip::<Subject>(value)?;
    }
    for value in &fixtures.invalid_subjects {
        refused::<Subject>(value)?;
    }
    for value in &fixtures.values {
        round_trip::<SettingValue>(value)?;
    }
    for value in &fixtures.invalid_values {
        refused::<SettingValue>(value)?;
    }
    for value in &fixtures.requests {
        round_trip::<SettingsRequest>(value)?;
    }
    for value in &fixtures.invalid_requests {
        refused::<SettingsRequest>(value)?;
    }
    for value in &fixtures.settings {
        round_trip::<RuntimeSettings>(value)?;
    }
    for value in &fixtures.invalid_settings {
        refused::<RuntimeSettings>(value)?;
    }
    for value in &fixtures.pages {
        round_trip::<Page>(value)?;
    }
    Ok(())
}

#[test]
fn a_turn_without_its_channel_is_refused() -> Result<()> {
    let event = serde_json::json!({
        "event_id": "500",
        "agent_id": "assistant",
        "user_id": "200",
        "audience_id": "discord-100-300",
        "channel_id": "300",
        "delivery_target_id": "300",
        "feedback_target_id": "300:500",
        "content": "hello",
    });
    serde_json::from_value::<ExternalEvent>(event.clone())?;
    let opened = serde_json::json!({
        "event_id": "runtime:runtime-pi:schedule:1",
        "turn_id": "turn-1",
        "state": "running",
        "duplicate": false,
        "channel_id": "300",
    });
    round_trip::<OpenedTurn>(&opened)?;
    let fixtures: TurnFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/turns.json"
    ))?;
    let request = fixtures.requests.first().context("a request fixture")?;
    for (mut value, refuse) in [
        (event, refused::<ExternalEvent> as fn(&Value) -> Result<()>),
        (opened, refused::<OpenedTurn>),
        (request.clone(), refused::<RuntimeTurnRequest>),
    ] {
        let object = value.as_object_mut().context("an object")?;
        object.insert("channel_id".to_owned(), Value::String(String::new()));
        refuse(&value)?;
        value
            .as_object_mut()
            .context("an object")?
            .remove("channel_id");
        refuse(&value)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct ActivityFixtures {
    activities: Vec<Value>,
    invalid_activities: Vec<Value>,
    lenient_controls: Vec<LenientControls>,
}

/// A snapshot whose controls a runtime may not send, and how core reads it.
#[derive(Deserialize)]
struct LenientControls {
    sent: Value,
    read: Value,
}

#[test]
fn shared_activity_wire_fixtures() -> Result<()> {
    let fixtures: ActivityFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/activity.json"
    ))?;
    for value in &fixtures.activities {
        round_trip::<TurnActivity>(value)?;
    }
    for value in &fixtures.invalid_activities {
        refused::<TurnActivity>(value)?;
    }
    // A bad control list never costs the snapshot: unknown and repeated
    // controls are skipped.
    for LenientControls { sent, read } in &fixtures.lenient_controls {
        let activity: TurnActivity = serde_json::from_value(sent.clone())?;
        assert_eq!(serde_json::to_value(activity)?, *read);
    }
    Ok(())
}

#[derive(Deserialize)]
struct TurnFixtures {
    requests: Vec<Value>,
}

/// Core produces these requests; the runtime's schema validates them.
#[test]
fn shared_runtime_turn_wire_fixtures() -> Result<()> {
    let fixtures: TurnFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/turns.json"
    ))?;
    for value in &fixtures.requests {
        round_trip::<RuntimeTurnRequest>(value)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct CommandFixtures {
    catalogs: Vec<Value>,
    invalid_catalogs: Vec<Value>,
}

#[test]
fn shared_command_wire_fixtures() -> Result<()> {
    let fixtures: CommandFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/commands.json"
    ))?;
    for value in &fixtures.catalogs {
        round_trip::<CommandCatalog>(value)?;
    }
    for value in &fixtures.invalid_catalogs {
        refused::<CommandCatalog>(value)?;
    }
    Ok(())
}

/// A runtime's final report is display data: a valid one is kept, and one
/// that is not valid is dropped without costing the turn its answer.
#[test]
fn an_invalid_final_report_never_invalidates_the_answer() -> Result<()> {
    let fixtures: ActivityFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/activity.json"
    ))?;
    let answer = |activity: &Value| serde_json::json!({ "outcome": "answered", "output": "Done.", "activity": activity });
    for value in &fixtures.activities {
        let TurnOutcome::Answered { activity, .. } = serde_json::from_value(answer(value))? else {
            anyhow::bail!("an answer read as another outcome");
        };
        assert_eq!(
            serde_json::to_value(activity)?,
            *value,
            "a valid report is kept as it was"
        );
    }
    for value in &fixtures.invalid_activities {
        let TurnOutcome::Answered { output, activity } = serde_json::from_value(answer(value))?
        else {
            anyhow::bail!("an answer read as another outcome");
        };
        assert_eq!(output, "Done.");
        assert!(activity.is_none());
    }
    Ok(())
}

#[test]
fn only_a_whole_readiness_says_a_runtime_can_restart() -> Result<()> {
    use carillon_protocol::model::RestartReadiness;
    for value in [
        serde_json::json!({"state": "ready"}),
        serde_json::json!({"state": "unready", "reason": "a fence"}),
    ] {
        round_trip::<RestartReadiness>(&value)?;
    }
    for value in [
        serde_json::json!({"state": "ready", "also": 1}),
        serde_json::json!({"state": "unready"}),
        serde_json::json!({"state": "unready", "reason": "a fence", "also": 1}),
        serde_json::json!({"state": "maybe"}),
        serde_json::json!({}),
    ] {
        refused::<RestartReadiness>(&value)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct HeldFixtures {
    outcomes: Vec<Value>,
    invalid_outcomes: Vec<Value>,
    openings: Vec<Value>,
    invalid_openings: Vec<Value>,
    controls: Vec<Value>,
    invalid_controls: Vec<Value>,
    running: Vec<Value>,
    invalid_running: Vec<Value>,
}

#[test]
fn shared_held_turn_wire_fixtures() -> Result<()> {
    let fixtures: HeldFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/held.json"
    ))?;
    for value in &fixtures.outcomes {
        round_trip::<TurnOutcome>(value)?;
    }
    for value in &fixtures.invalid_outcomes {
        refused::<TurnOutcome>(value)?;
    }
    for value in &fixtures.openings {
        round_trip::<TurnOpening>(value)?;
    }
    for value in &fixtures.invalid_openings {
        refused::<TurnOpening>(value)?;
    }
    for value in &fixtures.controls {
        round_trip::<RuntimeTurnControl>(value)?;
    }
    for value in &fixtures.invalid_controls {
        refused::<RuntimeTurnControl>(value)?;
    }
    for value in &fixtures.running {
        round_trip::<RunningTurn>(value)?;
    }
    for value in &fixtures.invalid_running {
        refused::<RunningTurn>(value)?;
    }
    Ok(())
}
