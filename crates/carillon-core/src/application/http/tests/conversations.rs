//! An interface's reads of its own conversations over HTTP: only an enabled
//! interface reads, and only what is its own.

use std::fs;

use carillon_protocol::conversations::{ConversationPage, Transcript};
use tempfile::tempdir;

use super::{Fixture, accept, answer_next, configured, installation};
use crate::application::http::*;

async fn read(
    state: &Fixture,
    provider: &str,
    token: Option<&str>,
    path: &str,
) -> Result<(StatusCode, Value)> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let mut request = Request::builder()
        .uri(path)
        .header(PROVIDER_HEADER, provider);
    if let Some(token) = token {
        request = request.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router(configured(state))
        .oneshot(request.body(Body::empty())?)
        .await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

/// The body of a read that succeeded, which says no more than `T` does.
fn exactly<T: DeserializeOwned + serde::Serialize>(
    (status, body): (StatusCode, Value),
) -> Result<T> {
    anyhow::ensure!(status == StatusCode::OK, "{status}: {body}");
    let parsed: T = serde_json::from_value(body.clone())?;
    assert_eq!(serde_json::to_value(&parsed)?, body);
    Ok(parsed)
}

const LIST: &str = "/v1/conversations";
/// The conversation with audience `web chat/1`, as a path names it.
const TRANSCRIPT: &str = "/v1/conversations/test-agent/web%20chat%2F1";

#[tokio::test]
async fn an_interface_reads_its_own_conversations_and_no_other_caller_reads_any() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let mut second = state.config.value.providers[0].clone();
    "second-interface".clone_into(&mut second.id);
    "http://127.0.0.1:9".clone_into(&mut second.url);
    second.token_hash = carillon_installation::config::hash_token("second");
    state.config.value.providers.push(second);
    fs::write(
        directory.path().join("credentials/second-interface.token"),
        "second",
    )?;
    state.config.value.validate()?;
    let receipt = accept(&state, "hello", "web chat/1").await?;
    answer_next(&configured(&state).core, "test-runtime").await?;
    let own = Some("test-only");

    let page: ConversationPage = exactly(read(&state, "test-interface", own, LIST).await?)?;
    let [conversation] = page.conversations.as_slice() else {
        anyhow::bail!("expected one conversation: {page:?}");
    };
    assert_eq!(
        (
            conversation.audience_id.as_str(),
            conversation.title.as_str(),
            conversation.at_work
        ),
        ("web chat/1", "hello", 0)
    );
    let below = format!("{LIST}?before={}", conversation.activity);
    let page: ConversationPage = exactly(read(&state, "test-interface", own, &below).await?)?;
    assert!(page.conversations.is_empty());
    let transcript: Transcript = exactly(read(&state, "test-interface", own, TRANSCRIPT).await?)?;
    let [turn] = transcript.turns.as_slice() else {
        anyhow::bail!("expected one turn: {transcript:?}");
    };
    assert_eq!(turn.turn_id, receipt.turn_id);
    assert_eq!(
        turn.answer.as_ref().map(|answer| answer.content.as_str()),
        Some("reply to hello")
    );

    let other = Some("second");
    let page: ConversationPage = exactly(read(&state, "second-interface", other, LIST).await?)?;
    assert!(page.conversations.is_empty());
    let transcript: Transcript =
        exactly(read(&state, "second-interface", other, TRANSCRIPT).await?)?;
    assert!(transcript.turns.is_empty());

    let foreign = format!("{TRANSCRIPT}?before={}", receipt.turn_id);
    let transcript: Transcript = exactly(read(&state, "second-interface", other, &foreign).await?)?;
    assert!(transcript.turns.is_empty());

    state.config.value.providers[2].enabled = false;
    for path in [LIST, TRANSCRIPT] {
        for (provider, token, refusal) in [
            ("test-interface", None, "component authentication required"),
            ("test-interface", other, "component authentication failed"),
            ("test-runtime", own, "unknown interface provider"),
            ("unknown", own, "unknown interface provider"),
            ("second-interface", other, "interface provider is disabled"),
        ] {
            assert_eq!(
                read(&state, provider, token, path).await?,
                (StatusCode::FORBIDDEN, json!({ "error": refusal })),
                "{provider} with {token:?} read {path}"
            );
        }
    }
    Ok(())
}
