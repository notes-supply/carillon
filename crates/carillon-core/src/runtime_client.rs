//! Concrete authenticated runtime transport. It does not claim or commit turns.
use crate::TurnFailure;
use anyhow::anyhow;
use carillon_protocol::model::{RuntimeTurnRequest, RuntimeTurnResponse};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

/// A runtime's failure response. Its `error` is phrased for the person using
/// the interface and safe to display; a runtime omits it when it has nothing
/// to explain.
#[derive(Deserialize)]
struct RuntimeFailure {
    error: String,
}

#[derive(Clone)]
pub struct RuntimeClient {
    client: Client,
    url: String,
    token: String,
}
impl RuntimeClient {
    /// Construct a transport using caller-owned HTTP settings and resolved credentials.
    #[must_use]
    pub fn new(client: Client, url: String, token: String) -> Self {
        Self { client, url, token }
    }
    /// Submit one request, distinguishing provably unreceived work from ambiguous execution.
    /// # Errors
    /// Returns transport, runtime or invalid response failures. Only `Retry` permits automatic
    /// replay of executable work; `Unanswered` also permits asking again for an observation.
    pub async fn execute(
        &self,
        request: RuntimeTurnRequest,
    ) -> Result<RuntimeTurnResponse, TurnFailure> {
        let response = self
            .client
            .post(format!("{}/v1/turns", self.url))
            .bearer_auth(&self.token)
            .timeout(Duration::from_mins(30))
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                // A connect failure means the request never reached the runtime.
                // Any later failure — timeout, broken transfer — may have left
                // model work running, so the outcome is ambiguous.
                let retry = error.is_connect();
                let failure = anyhow!(error).context("contact runtime provider");
                if retry {
                    TurnFailure::Retry(failure)
                } else {
                    TurnFailure::Unanswered(failure)
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            // A runtime's `error` explains the failure for the conversation;
            // its diagnostics stay in the runtime's own log.
            let explanation = response
                .json::<RuntimeFailure>()
                .await
                .ok()
                .map(|failure| failure.error)
                .filter(|explanation| !explanation.trim().is_empty());
            return Err(match explanation {
                Some(explanation) => TurnFailure::Explained(explanation),
                // A closing runtime gives no answer; work may still have run.
                None if status == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                    TurnFailure::Unanswered(anyhow!("runtime provider closed before answering"))
                }
                None => TurnFailure::Uncertain(anyhow!(
                    "runtime provider failed the turn with {status} and no explanation"
                )),
            });
        }
        // A body cut off or timed out is no answer; a body received whole
        // that is not a valid response is a failed turn.
        let body = response.bytes().await.map_err(|error| {
            TurnFailure::Unanswered(anyhow!(error).context("read runtime response"))
        })?;
        let response = serde_json::from_slice::<RuntimeTurnResponse>(&body).map_err(|error| {
            TurnFailure::Uncertain(anyhow!(error).context("decode runtime response"))
        })?;
        crate::engine::validate_response(&request.correlation_id, &response)?;
        Ok(response)
    }
}
