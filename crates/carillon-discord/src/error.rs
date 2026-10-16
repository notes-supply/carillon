//! Admission and core-transport failures.
use axum::http::StatusCode;
use tracing::error;
use twilight_http::{
    error::ErrorType as DiscordErrorType, response::StatusCode as DiscordStatusCode,
};
#[derive(Debug)]
pub struct InterfaceError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl InterfaceError {
    pub(crate) fn transient(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }

    pub(crate) fn protocol(message: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message,
        }
    }

    /// Whether the failure is worth retrying: core unreachable or failing,
    /// rather than rejecting the request, refusing it for lack of storage,
    /// or violating its wire contract.
    #[must_use]
    pub(crate) fn is_transient(&self) -> bool {
        self.status.is_server_error()
            && !matches!(
                self.status,
                StatusCode::BAD_GATEWAY | StatusCode::INSUFFICIENT_STORAGE
            )
    }
}

impl From<anyhow::Error> for InterfaceError {
    fn from(error: anyhow::Error) -> Self {
        error!(%error, "component request failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for InterfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for InterfaceError {}

pub(crate) fn discord_status_is_terminal(status: DiscordStatusCode) -> bool {
    status.is_client_error() && status != DiscordStatusCode::TOO_MANY_REQUESTS
}

pub(crate) fn discord_failure_is_terminal(failure: &anyhow::Error) -> bool {
    failure
        .downcast_ref::<twilight_http::Error>()
        .is_some_and(|failure| match failure.kind() {
            DiscordErrorType::Response { status, .. } => discord_status_is_terminal(*status),
            DiscordErrorType::BuildingRequest
            | DiscordErrorType::CreatingHeader { .. }
            | DiscordErrorType::Json
            | DiscordErrorType::Unauthorized
            | DiscordErrorType::Validation => true,
            _ => false,
        })
}
