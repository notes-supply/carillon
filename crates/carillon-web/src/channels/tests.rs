//! What `carillon settings` is told: a web chat has no settings page.

use anyhow::Result;
use axum::{
    body::Body,
    http::{Method, StatusCode, header::CONTENT_TYPE},
};
use serde_json::json;

use crate::fixture::{Fixture, conversation, operator};

#[tokio::test]
async fn no_web_chat_has_a_settings_page_of_its_own() -> Result<()> {
    let fixture = Fixture::new().await?;
    let chat = "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b";
    fixture
        .core()
        .conversations
        .push(conversation("assistant", chat, 1));
    let listed = fixture
        .send(operator(Method::GET, "/v1/channels").body(Body::empty())?)
        .await?;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.json()?, json!([]));
    for channel in [chat, "300", "dm"] {
        let settings = format!("/v1/channels/{channel}/settings");
        for request in [
            operator(Method::GET, &settings).body(Body::empty())?,
            operator(Method::PUT, &settings)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"setting": "model", "value": null, "expected": "ab".repeat(32)})
                        .to_string(),
                ))?,
        ] {
            let answer = fixture.send(request).await?;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{channel}");
            let said = answer.json()?["error"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            assert!(said.contains("carillon settings --agent"), "{said}");
        }
    }
    let core = fixture.core();
    assert!(
        core.settings.is_empty() && core.asked.is_empty(),
        "core was asked"
    );
    Ok(())
}
