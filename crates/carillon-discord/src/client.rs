//! Authenticated transport for the core provider API; no service startup.
use crate::InterfaceError;
use anyhow::Result;
use carillon_installation::loopback;
use carillon_protocol::model::{
    CommandCatalog, Delivery, DeliveryAck, DeliveryOutcome, DeliveryReconciliation, EventReceipt,
    EventStatus, ExternalEvent, INSTANCE_HEADER, Job, JobPage, ModelQuery, ModelUpdate,
    PROVIDER_HEADER,
};
use carillon_protocol::models::ModelState;
use reqwest::{Client, IntoUrl, Method, RequestBuilder, Url};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;
/// The provider's authenticated client for core's provider API.
#[derive(Clone)]
pub struct CoreClient {
    http: Client,
    base_url: String,
    token: String,
    provider_id: String,
    /// Fresh per process start; core releases delivery claims held under
    /// other, presumed dead, instance identities during reconciliation.
    instance_id: String,
}

impl CoreClient {
    /// # Errors
    /// Returns an error when the HTTP client cannot be initialized.
    pub fn new(base_url: String, provider_id: String, token: String) -> Result<Self> {
        Ok(Self {
            http: loopback::client().build()?,
            base_url,
            token,
            provider_id,
            instance_id: Uuid::now_v7().to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn request(&self, method: Method, url: impl IntoUrl) -> RequestBuilder {
        self.http
            .request(method, url)
            // These are local admission/delivery endpoints, not model calls.
            // Bound headers and body reads so ambiguous claims can reconcile.
            .timeout(Duration::from_secs(10))
            .bearer_auth(&self.token)
            .header(PROVIDER_HEADER, &self.provider_id)
            .header(INSTANCE_HEADER, &self.instance_id)
    }

    /// Submit an event with its stable platform event ID. With attachments,
    /// core answers once it has captured them, so the wait is longer than
    /// core's own capture deadline; other calls keep their short one.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn submit_event(
        &self,
        event: &ExternalEvent,
    ) -> Result<EventReceipt, InterfaceError> {
        let mut request = self
            .request(Method::POST, self.url("/v1/events"))
            .json(event);
        if event.attachments.is_some() {
            request = request.timeout(CAPTURE_SUBMISSION_TIMEOUT);
        }
        core_json(request, "submit event to core").await
    }

    /// What core knows about one of this provider's events.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn event_status(&self, event_id: &str) -> Result<EventStatus, InterfaceError> {
        let mut url = Url::parse(&self.url("/v1/events"))
            .map_err(|error| InterfaceError::protocol(error.to_string()))?;
        url.path_segments_mut()
            .map_err(|()| InterfaceError::protocol("core URL cannot have a path".to_owned()))?
            .push(event_id);
        core_json(
            self.request(Method::GET, url),
            "read event status from core",
        )
        .await
    }

    /// Claim the next delivery for this provider instance.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn claim_delivery(&self) -> Result<Option<Delivery>, InterfaceError> {
        let request = self.request(Method::GET, self.url("/v1/deliveries"));
        core_json(request, "claim core delivery").await
    }

    /// Claim the delivery of one turn for this provider instance, if it has
    /// one pending or held.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn claim_turn_delivery(
        &self,
        turn_id: &str,
    ) -> Result<Option<Delivery>, InterfaceError> {
        let mut url = Url::parse(&self.url("/v1/deliveries"))
            .map_err(|error| InterfaceError::protocol(error.to_string()))?;
        url.query_pairs_mut().append_pair("turn_id", turn_id);
        core_json(self.request(Method::GET, url), "claim core delivery").await
    }

    /// Commit the observed outcome of a held delivery claim.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn acknowledge_delivery(
        &self,
        id: i64,
        outcome: DeliveryOutcome,
        native_ref: Option<String>,
    ) -> Result<(), InterfaceError> {
        let request = self
            .request(Method::POST, self.url(&format!("/v1/deliveries/{id}/ack")))
            .json(&DeliveryAck {
                native_ref,
                outcome,
            });
        core_send(request, "acknowledge core delivery").await?;
        Ok(())
    }

    /// Reconcile abandoned claims after establishing instance exclusivity.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn reconcile(&self) -> Result<DeliveryReconciliation, InterfaceError> {
        let request = self.request(Method::POST, self.url("/v1/deliveries/reconcile"));
        core_json(request, "reconcile abandoned delivery claims").await
    }

    /// A page of this provider's jobs at work, `after` the previous page,
    /// each running turn with its runtime's latest snapshot when it has one.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn jobs_at_work(&self, after: Option<&str>) -> Result<JobPage, InterfaceError> {
        let mut url = Url::parse(&self.url("/v1/jobs"))
            .map_err(|error| InterfaceError::protocol(error.to_string()))?;
        if let Some(after) = after {
            url.query_pairs_mut().append_pair("after", after);
        }
        core_json(
            self.request(Method::GET, url),
            "read jobs at work from core",
        )
        .await
    }

    /// The jobs this provider names by their origin turn, at most
    /// [`JOB_READ_LIMIT`](carillon_protocol::model::JOB_READ_LIMIT), however
    /// they stand; core leaves out a name of nothing this provider began.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn named_jobs(&self, named: &[String]) -> Result<Vec<Job>, InterfaceError> {
        let mut url = Url::parse(&self.url("/v1/jobs"))
            .map_err(|error| InterfaceError::protocol(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("tracked", &named.join(","));
        let page: JobPage =
            core_json(self.request(Method::GET, url), "read named jobs from core").await?;
        Ok(page.jobs)
    }

    pub(crate) fn authenticates(&self, token: &str) -> bool {
        carillon_installation::config::token_matches(
            &carillon_installation::config::hash_token(&self.token),
            token,
        )
    }

    /// Read the runtime's model state for an agent and optional conversation.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn model_state(&self, query: &ModelQuery) -> Result<ModelState, String> {
        self.runtime_query("/v1/model", query).await
    }

    /// The runtime's own commands a person may run in the conversation.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn commands(&self, query: &ModelQuery) -> Result<CommandCatalog, String> {
        self.runtime_query("/v1/commands", query).await
    }

    async fn runtime_query<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &ModelQuery,
    ) -> Result<T, String> {
        let mut url = Url::parse(&self.url(path)).map_err(|error| error.to_string())?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("agent_id", &query.agent_id);
            if let Some(audience_id) = &query.audience_id {
                pairs.append_pair("audience_id", audience_id);
            }
        }
        let request = self.request(Method::GET, url).timeout(MODEL_CALL_TIMEOUT);
        runtime_json(request).await
    }

    /// Changes a model selection and returns the resulting state.
    ///
    /// # Errors
    /// Returns core rejection, transport, or malformed-response failures.
    pub async fn set_model(&self, update: &ModelUpdate) -> Result<ModelState, String> {
        let request = self
            .request(Method::PUT, self.url("/v1/model"))
            .timeout(MODEL_CALL_TIMEOUT)
            .json(update);
        runtime_json(request).await
    }
}

/// Core captures an event's files for at most five minutes before it answers.
const CAPTURE_SUBMISSION_TIMEOUT: Duration = Duration::from_mins(6);

/// Model and command calls answer a waiting user and run inside the command
/// module's deadline, so they give up well before it.
const MODEL_CALL_TIMEOUT: Duration = Duration::from_secs(6);
/// What the user is told when a model or command call ran out of time.
pub(crate) const RUNTIME_LATE: &str = "The runtime did not answer in time.";

/// Performs a model or command call and turns every failure into words for
/// the user. Core and the runtime phrase their rejections for people;
/// transport failures are described here.
async fn runtime_json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, String> {
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Err(RUNTIME_LATE.to_owned());
        }
        Err(error) => {
            warn!(%error, "model or command call failed to reach core");
            return Err("The runtime is not reachable right now.".to_owned());
        }
    };
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|_| "The runtime gave an unusable answer.".to_owned());
    }
    let detail = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
        .filter(|detail| !detail.is_empty());
    warn!(%status, detail, "model or command call was rejected");
    Err(detail.unwrap_or_else(|| "The runtime refused the request.".to_owned()))
}

async fn core_send(
    request: RequestBuilder,
    action: &str,
) -> Result<reqwest::Response, InterfaceError> {
    let response = request
        .send()
        .await
        .map_err(|error| InterfaceError::transient(format!("{action}: {error}")))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let detail = response.text().await.unwrap_or_default();
    Err(InterfaceError {
        status,
        message: if detail.is_empty() {
            format!("{action}: core responded with {status}")
        } else {
            format!("{action}: {detail}")
        },
    })
}

async fn core_json<T: DeserializeOwned>(
    request: RequestBuilder,
    action: &str,
) -> Result<T, InterfaceError> {
    let bytes = core_send(request, action)
        .await?
        .bytes()
        .await
        .map_err(|error| InterfaceError::transient(format!("read {action}: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| InterfaceError::protocol(format!("decode {action}: {error}")))
}

#[cfg(test)]
pub(crate) fn test_client(base_url: String) -> Result<CoreClient> {
    let mut client = CoreClient::new(base_url, "discord".to_owned(), "token".to_owned())?;
    client.instance_id = "instance".to_owned();
    Ok(client)
}

#[cfg(test)]
mod tests;
