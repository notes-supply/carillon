//! The provider process: one router behind one gate, and an ordered stop.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, bail};
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
use carillon_installation::config::{LoadedConfig, component_directory};
use carillon_interface::{CoreClient, InterfaceError, Started, authorize_operator, bearer, start};
use carillon_protocol::model::Health;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;

use crate::{
    api,
    browser::{self, Browser, refusal},
    channels, files,
    site::{self, INDEX, Site},
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
    pub(crate) site: Site,
}

pub(crate) struct ServeOptions {
    pub(crate) provider_id: String,
    pub(crate) token_file: PathBuf,
    /// The release's copy of the page.
    pub(crate) site: PathBuf,
}

pub(crate) async fn serve(config: LoadedConfig, options: ServeOptions) -> Result<()> {
    let Some(Started {
        provider,
        listener,
        core,
        health,
    }) = start(&config, &options.provider_id, &options.token_file).await?
    else {
        return Ok(());
    };
    if !options.site.join(INDEX).is_file() {
        bail!(
            "the web page is not at {}; name its directory with --site",
            options.site.display()
        );
    }
    let data_dir = &config.value.data_dir;
    let authority = provider
        .url
        .strip_prefix("http://")
        .context("provider URL must use http://")?
        .to_owned();
    let web = Arc::new(Web {
        core,
        health,
        browser: Browser::open(browser::key_file(data_dir, &provider.id), authority)?,
        site: Site {
            custom: component_directory(data_dir, &provider.id).join("site"),
            release: options.site,
        },
        registry: config,
    });
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    info!(url = %provider.url, "web interface provider ready");
    axum::serve(listener, router(web))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
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
    let operator = channels::routes()
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

/// The page may load only its own scripts, styles and images, and talk only
/// to this provider.
const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    img-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; \
    frame-ancestors 'none'";

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

async fn browser_only(
    State(web): State<Arc<Web>>,
    request: Request,
    next: Next,
) -> Result<Response, InterfaceError> {
    if !web.browser.authenticates(bearer(request.headers())) {
        return Err(refusal(
            StatusCode::UNAUTHORIZED,
            "This browser is not signed in. Run `carillon web open`.",
        ));
    }
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
