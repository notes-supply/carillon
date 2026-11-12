use crate::InterfaceError;
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use carillon_installation::config::{Config, token_matches};

/// The bearer token `headers` present, empty when they present none.
#[must_use]
pub fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
}

/// Operator actions need the operator credential, not the interface's. The
/// provider never holds it: the bearer's hash is compared with the
/// registry's.
///
/// # Errors
///
/// Returns a 403 for any other bearer.
pub fn authorize_operator(registry: &Config, headers: &HeaderMap) -> Result<(), InterfaceError> {
    if !token_matches(&registry.core.operator_token_hash, bearer(headers)) {
        return Err(InterfaceError {
            status: StatusCode::FORBIDDEN,
            message: "this endpoint requires the operator credential".to_owned(),
        });
    }
    Ok(())
}
