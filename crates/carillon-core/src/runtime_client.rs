//! Concrete authenticated runtime transport. It does not claim or commit turns.
use crate::TurnFailure;
use anyhow::anyhow;
use carillon_protocol::model::{RuntimeTurnRequest, RuntimeTurnResponse};
use reqwest::Client;
use std::time::Duration;

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
    /// Returns transport, runtime or invalid response failures. Only `Retry` permits automatic replay.
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
                    TurnFailure::Uncertain(failure)
                }
            })?
            .error_for_status()
            .map_err(|error| {
                TurnFailure::Uncertain(anyhow!(error).context("runtime provider rejected turn"))
            })?
            .json::<RuntimeTurnResponse>()
            .await
            .map_err(|error| {
                TurnFailure::Uncertain(anyhow!(error).context("decode runtime response"))
            })?;
        crate::engine::validate_response(&request.correlation_id, &response)?;
        Ok(response)
    }
}
