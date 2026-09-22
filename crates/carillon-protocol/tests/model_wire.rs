//! Both languages consume the same wire examples, including canonical round trips.
use anyhow::Result;
use carillon_protocol::{
    model::{RuntimeModelQuery, RuntimeModelUpdate},
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
