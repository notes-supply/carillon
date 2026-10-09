//! Both languages consume the same wire examples, including canonical round trips.
use anyhow::Result;
use carillon_protocol::{
    model::{
        CommandCatalog, RuntimeModelQuery, RuntimeModelUpdate, RuntimeTurnRequest, TurnActivity,
    },
    models::ModelState,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

fn round_trip<T: DeserializeOwned + Serialize>(value: &Value) -> Result<()> {
    let parsed: T = serde_json::from_value(value.clone())?;
    assert_eq!(serde_json::to_value(parsed)?, *value);
    Ok(())
}

#[derive(Deserialize)]
struct Fixtures {
    states: Vec<Value>,
    queries: Vec<Value>,
    updates: Vec<Value>,
    invalid_updates: Vec<Value>,
    invalid_queries: Vec<Value>,
    invalid_states: Vec<Value>,
    accepted_updates: Vec<Value>,
}

#[test]
fn shared_model_wire_fixtures() -> Result<()> {
    let fixtures: Fixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/models.json"
    ))?;
    for value in &fixtures.states {
        round_trip::<ModelState>(value)?;
    }
    for value in &fixtures.queries {
        round_trip::<RuntimeModelQuery>(value)?;
    }
    for value in &fixtures.updates {
        round_trip::<RuntimeModelUpdate>(value)?;
    }
    for value in &fixtures.invalid_updates {
        assert!(serde_json::from_value::<RuntimeModelUpdate>(value.clone()).is_err());
    }
    for value in fixtures.invalid_queries {
        assert!(serde_json::from_value::<RuntimeModelQuery>(value).is_err());
    }
    for value in fixtures.invalid_states {
        assert!(serde_json::from_value::<ModelState>(value).is_err());
    }
    for value in fixtures.accepted_updates {
        serde_json::from_value::<RuntimeModelUpdate>(value)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct ActivityFixtures {
    activities: Vec<Value>,
    invalid_activities: Vec<Value>,
}

#[test]
fn shared_activity_wire_fixtures() -> Result<()> {
    let fixtures: ActivityFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/activity.json"
    ))?;
    for value in &fixtures.activities {
        round_trip::<TurnActivity>(value)?;
    }
    for value in fixtures.invalid_activities {
        assert!(serde_json::from_value::<TurnActivity>(value).is_err());
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
    for value in fixtures.invalid_catalogs {
        assert!(serde_json::from_value::<CommandCatalog>(value).is_err());
    }
    Ok(())
}

/// A runtime's final report is display data: a valid one is kept, and one
/// that is not valid is dropped without costing the turn its answer.
#[test]
fn an_invalid_final_report_never_invalidates_the_answer() -> Result<()> {
    use carillon_protocol::model::RuntimeTurnResponse;
    let fixtures: ActivityFixtures = serde_json::from_str(include_str!(
        "../../../packages/protocol/fixtures/activity.json"
    ))?;
    let answer = |activity: &Value| serde_json::json!({ "correlation_id": "turn", "output": "Done.", "activity": activity });
    for value in &fixtures.activities {
        let response: RuntimeTurnResponse = serde_json::from_value(answer(value))?;
        assert_eq!(
            serde_json::to_value(response.activity)?,
            *value,
            "a valid report is kept as it was"
        );
    }
    for value in &fixtures.invalid_activities {
        let response: RuntimeTurnResponse = serde_json::from_value(answer(value))?;
        assert_eq!(response.output, "Done.");
        assert!(response.activity.is_none());
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
        assert!(
            serde_json::from_value::<RestartReadiness>(value.clone()).is_err(),
            "{value}"
        );
    }
    Ok(())
}
