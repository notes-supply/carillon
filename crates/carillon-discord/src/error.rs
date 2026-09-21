//! Admission and core-transport failures.
use axum::http::StatusCode;
use tracing::error;
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
    /// rather than rejecting the request or violating its wire contract.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        self.status.is_server_error() && self.status != StatusCode::BAD_GATEWAY
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
