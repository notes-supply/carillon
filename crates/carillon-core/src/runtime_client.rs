//! Concrete authenticated runtime transport. It does not claim or commit turns.
use crate::{InboxFailure, TurnFailure};
use anyhow::anyhow;
use carillon_protocol::model::{RestartReadiness, RuntimeTurnRequest, RuntimeTurnResponse};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::time::Duration;

/// How long a durable runtime may take to keep a turn handed to it before
/// it is handed over again.
const INBOX_TIMEOUT: Duration = Duration::from_secs(10);

/// A runtime's failure response. Its `error` is phrased for the person using
/// the interface and safe to display; a runtime omits it when it has nothing
/// to explain. Its `activity` is the turn's final report, which a runtime
/// sends whether or not it explains: an invalid one is dropped.
#[derive(Default, Deserialize)]
struct RuntimeFailure {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    activity: Option<serde_json::Value>,
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
    ///
    /// The request has no total deadline: a turn takes as long as its
    /// runtime's work, which may wait on native work for hours. A caller
    /// that stops waiting, such as a worker stopped with the service, ends the
    /// request, and the runtime takes the closed connection as cancellation.
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
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                // A connect failure means the request never reached the runtime.
                // Any later failure, such as a broken transfer, may have left
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
            let failure = response.json::<RuntimeFailure>().await.unwrap_or_default();
            let explanation = failure
                .error
                .filter(|explanation| !explanation.trim().is_empty());
            let activity = failure
                .activity
                .and_then(|activity| serde_json::from_value(activity).ok());
            return Err(match (explanation, activity) {
                // A closing runtime gives no answer; work may still have run.
                (None, _) if status == StatusCode::SERVICE_UNAVAILABLE => {
                    TurnFailure::Unanswered(anyhow!("runtime provider closed before answering"))
                }
                (None, None) => TurnFailure::Uncertain(anyhow!(
                    "runtime provider failed the turn with {status} and no explanation"
                )),
                (explanation, activity) => TurnFailure::Reported(Box::new(crate::Reported {
                    status: status.as_u16(),
                    explanation,
                    activity,
                })),
            });
        }
        // A body cut off is no answer; a body received whole
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

    /// Hands a durable runtime a turn to hold, which it settles later. It
    /// takes a turn it already holds as the same one, so a turn not known to
    /// be taken is handed over again.
    /// # Errors
    /// Returns `Later` for a turn that may not have arrived or that the
    /// runtime asked to have later (5xx, 408 or 429), `Misinstalled` for an
    /// answer no turn explains (401, 403, 404, 405 or another 2xx), and
    /// `Refused` for any other.
    pub async fn submit(&self, request: &RuntimeTurnRequest) -> Result<(), InboxFailure> {
        let response = self
            .client
            .post(format!("{}/v1/inbox", self.url))
            .bearer_auth(&self.token)
            .timeout(INBOX_TIMEOUT)
            .json(request)
            .send()
            .await
            .map_err(|error| {
                InboxFailure::Later(anyhow!(error).context("contact runtime provider"))
            })?;
        let status = response.status();
        if status == StatusCode::ACCEPTED {
            return Ok(());
        }
        if status.is_server_error()
            || matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(InboxFailure::Later(anyhow!(
                "runtime provider answered {status}"
            )));
        }
        if status.is_success()
            || matches!(
                status,
                StatusCode::UNAUTHORIZED
                    | StatusCode::FORBIDDEN
                    | StatusCode::NOT_FOUND
                    | StatusCode::METHOD_NOT_ALLOWED
            )
        {
            return Err(InboxFailure::Misinstalled(anyhow!(
                "runtime provider answered {status} at /v1/inbox"
            )));
        }
        let failure = response.json::<RuntimeFailure>().await.unwrap_or_default();
        Err(InboxFailure::Refused {
            status: status.as_u16(),
            explanation: failure
                .error
                .filter(|explanation| !explanation.trim().is_empty()),
        })
    }

    /// Whether the runtime says restarting it now loses nothing it alone
    /// knows. It answers at once from its own state.
    /// # Errors
    /// Returns transport failures and answers that are not a readiness.
    pub async fn restart_readiness(&self) -> anyhow::Result<RestartReadiness> {
        Ok(self
            .client
            .get(format!("{}/v1/restart", self.url))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}
