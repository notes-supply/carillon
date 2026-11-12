//! The provider process: one router behind one gate, and an ordered stop.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{
        HeaderName, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS},
    },
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use carillon_installation::config::{LoadedConfig, attachment_staging, component_directory};
use carillon_interface::{
    CoreClient, InterfaceError, Staging, Started, authorize_operator, bearer, start,
};
use carillon_protocol::{model::Health, settings::Page};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    api,
    browser::{self, Browser, Key, refusal},
    files,
    send::Sends,
    site::{self, Site},
};

/// What every handler shares. Nothing here is a conversation's: core keeps
/// those.
pub(crate) struct Web {
    pub(crate) core: CoreClient,
    /// The registry the provider started with: its agents, the operator's
    /// credential hash and core's limits on a message's files.
    pub(crate) registry: LoadedConfig,
    pub(crate) health: Health,
    pub(crate) browser: Browser,
    pub(crate) staging: Staging,
    pub(crate) sends: Sends,
    pub(crate) site: Site,
    /// The installing user's login name, the `user_id` of every message.
    pub(crate) user: String,
    /// Cancelled when the provider is asked to stop.
    pub(crate) stopping: CancellationToken,
}

/// Serves provider `provider_id` of `config` with the page in `site`, the
/// release's copy, until it is asked to stop.
pub(crate) async fn serve(
    config: LoadedConfig,
    provider_id: &str,
    token_file: &Path,
    site: PathBuf,
) -> Result<()> {
    // Read before anything slow, so a code made once this process runs is good.
    let started = browser::now_ms();
    let Some(Started {
        provider,
        listener,
        core,
        health,
    }) = start(&config, provider_id, token_file).await?
    else {
        return Ok(());
    };
    if !site::has_page(&site) {
        bail!(
            "the web page is not at {}; name its directory with --site",
            site.display()
        );
    }
    let data_dir = &config.value.data_dir;
    let staging = Staging::open(attachment_staging(data_dir, &provider.id))?;
    let stopping = CancellationToken::new();
    let web = Arc::new(Web {
        core,
        health,
        browser: Browser {
            key_file: browser::key_file(data_dir, &provider.id),
            // A browser names the address in one spelling whatever the registry's.
            authority: listener.local_addr()?.to_string(),
            started,
            used: Mutex::default(),
        },
        staging,
        sends: Sends::default(),
        site: Site {
            custom: component_directory(data_dir, &provider.id).join("site"),
            release: site,
        },
        user: std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "owner".to_owned()),
        stopping: stopping.clone(),
        registry: config,
    });
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    info!(url = %web.browser.origin(), "web interface provider ready");
    axum::serve(listener, router(web))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
            stopping.cancel();
        })
        .await?;
    Ok(())
}

/// Every route. The gate wraps them all, the fallback included, so a route
/// added here cannot be reached around it. A route added under `/api` needs
/// the browser's token and one under `/v1` the operator's credential,
/// whether or not it remembers to ask.
pub(crate) fn router(web: Arc<Web>) -> Router {
    // Each has a fallback of its own, inside its layer: a path that is no
    // route is refused like one that is, and never reaches the page's files.
    let api = api::routes()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(from_fn_with_state(Arc::clone(&web), browser_only));
    // `carillon settings` asks every interface for its channels' pages. A
    // web chat has none: it runs its agent's model.
    let operator = Router::new()
        .route("/channels", get(|| async { Json(Vec::<Page>::new()) }))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(from_fn_with_state(Arc::clone(&web), operator_only));
    Router::new()
        .route("/health", get(health))
        .route("/api/sign-in", post(api::sign_in))
        .nest("/api", api)
        .nest("/v1", operator)
        .route("/files/{*path}", get(files::file))
        .fallback(site::file)
        .layer(from_fn_with_state(Arc::clone(&web), gate))
        .with_state(web)
}

async fn health(State(web): State<Arc<Web>>) -> Json<Health> {
    Json(web.health.clone())
}

const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    img-src 'self'; font-src 'self'; connect-src 'self'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

/// Refuses a request that does not name this provider or comes from
/// another origin, before anything reads it, and gives every answer the
/// headers that keep it on this origin. No answer has a CORS header.
async fn gate(State(web): State<Arc<Web>>, request: Request, next: Next) -> Response {
    let mut response = match web.browser.admit(request.headers()) {
        Ok(()) => next.run(request).await,
        Err(refused) => refused.into_response(),
    };
    for (name, value) in [
        (CONTENT_SECURITY_POLICY, PAGE_POLICY),
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (REFERRER_POLICY, "no-referrer"),
        (CACHE_CONTROL, "no-store"),
        (
            HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
    ] {
        response
            .headers_mut()
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}

/// Lets a request with the browser's token through, with the key that
/// token was checked against: what the answer signs, it signs with that key.
async fn browser_only(
    State(web): State<Arc<Web>>,
    mut request: Request,
    next: Next,
) -> Result<Response, InterfaceError> {
    let key = Key::read(&web.browser.key_file);
    let Some(key) = key.filter(|key| key.authenticates(bearer(request.headers()))) else {
        return Err(refusal(
            StatusCode::UNAUTHORIZED,
            "This browser is not signed in. Run `carillon web open`.",
        ));
    };
    request.extensions_mut().insert(key);
    Ok(next.run(request).await)
}

async fn operator_only(
    State(web): State<Arc<Web>>,
    request: Request,
    next: Next,
) -> Result<Response, InterfaceError> {
    authorize_operator(&web.registry.value, request.headers())?;
    Ok(next.run(request).await)
}

#[cfg(test)]
pub(crate) mod tests;
