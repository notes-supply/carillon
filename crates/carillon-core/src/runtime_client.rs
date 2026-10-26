//! Concrete authenticated runtime transport. It records nothing of turns.
use crate::InboxFailure;
use anyhow::anyhow;
use carillon_protocol::model::{RestartReadiness, RuntimeTurnRequest};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::time::Duration;

/// How long a runtime may take to keep a turn handed to it before it is
/// handed over again.
const INBOX_TIMEOUT: Duration = Duration::from_secs(10);

/// A runtime's refusal. Its `error` is phrased for the person using the
/// interface and safe to display; a runtime omits it when it has nothing to
/// explain.
#[derive(Default, Deserialize)]
struct RuntimeFailure {
    #[serde(default)]
    error: Option<String>,
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
    /// Hands a runtime a turn to hold, which it settles later. It
    /// takes a turn it already holds as the same one, so a turn not known to
    /// be taken is handed over again.
    /// # Errors
    /// Returns `Later` for a turn that may not have arrived or that the
    /// runtime asked to have later (5xx, 408 or 429), `Misinstalled` for an
    /// answer no turn explains (400, 401, 403, 404, 405 or another 2xx), and
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
                StatusCode::BAD_REQUEST
                    | StatusCode::UNAUTHORIZED
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
