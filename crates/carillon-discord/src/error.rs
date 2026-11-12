use twilight_http::{
    error::ErrorType as DiscordErrorType, response::StatusCode as DiscordStatusCode,
};

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
