//! The provider process: one router behind one gate, and an ordered stop.

use std::{
    path::{Path, PathBuf},
    pin::pin,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{
        HeaderName, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS},
    },
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse as _, Response},
    routing::{get, post},
};
use carillon_installation::config::{
    DeliveryMode, LoadedConfig, Provider, attachment_staging, component_directory,
};
use carillon_interface::{
    CoreClient, InterfaceError, Staging, Started, authorize_operator, bearer, start,
};
use carillon_protocol::{model::Health, settings::Page};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    server::graceful::GracefulShutdown,
};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;
use tower_http::timeout::{RequestBodyTimeoutLayer, TimeoutLayer};
use tracing::{info, warn};

use crate::{
    api,
    browser::{self, Browser, Key, refusal},
    files,
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
    pub(crate) site: Site,
    /// The installing user's login name, the `user_id` of every message.
    pub(crate) user: String,
    /// Cancelled when the provider is asked to stop.
    pub(crate) stopping: CancellationToken,
}

/// Refuses an entry whose answers core would hold for an interface to claim
/// and send: the web provider claims none, so every restart would wait for
/// them.
pub(crate) fn retained(entry: &Provider) -> Result<()> {
    if entry.delivery != DeliveryMode::Retained {
        bail!(
            "{} needs the line `delivery = \"retained\"` in its entry in carillon.toml: a web \
             chat's answers are read from core, and nothing else delivers them",
            entry.id
        );
    }
    Ok(())
}

/// Serves provider `provider_id` of `config` with the page in `site`, the
/// release's copy, until it is asked to stop.
pub(crate) async fn serve(
    config: LoadedConfig,
    provider_id: &str,
    token_file: &Path,
    site: PathBuf,
) -> Result<()> {
    let Some(Started {
        provider,
        listener,
        core,
        health,
    }) = start(&config, provider_id, token_file).await?
    else {
        return Ok(());
    };
    retained(&provider)?;
    let addresses = browser::addresses(&config, &provider.id)?;
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
        browser: Browser::new(
            data_dir,
            provider.id.clone(),
            // A browser names the address in one spelling whatever the registry's.
            listener.local_addr()?.to_string(),
            addresses,
        ),
        staging,
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
    info!(url = %web.browser.listen.origin, "web interface provider ready");
    let asked_to_stop = async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    };
    Ok(until_stopped(listener, web, asked_to_stop).await?)
}

/// How long the requests in flight have to be answered once the provider
/// is told to stop.
const DRAIN: Duration = Duration::from_secs(2);

/// The most connections the provider serves at once. A peer cannot take
/// every descriptor and shut the owner out, and the provider keeps
/// descriptors free for core and for staged files. This is far more than a
/// person opens.
const MAX_CONNECTIONS: usize = 512;

/// How much of a connection hyper buffers. A request's line and headers must
/// fit in it, and a body is read in frames that fit in what the buffer grows
/// to, under twice this, which bounds how far the provider reads past a
/// multipart window.
const READ_BUFFER: usize = 64 * 1024;

/// How long a connection has to send a request's line and headers before it
/// is closed, so a peer that opens one and sends nothing, or sends it a byte
/// at a time, does not hold it open.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a request's body may send nothing before the request fails and
/// its connection closes, so a peer that sends a request's headers and then
/// nothing does not hold the connection open. It bounds each pause, not the
/// whole body: an upload from a phone on a weak network pauses and goes on.
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a sign-in has, once its headers are read, to arrive whole and be
/// checked. Anyone who can reach the provider can send one, and a body sent a
/// byte at a time is never idle, so this bounds how long a peer without the
/// password holds a connection. A guess is a few bytes and its check a second.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the provider waits after a failed accept, such as one refused
/// for want of a descriptor, so the failure is not retried in a busy loop.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// Serves `web` on `listener` until `asked_to_stop` completes, then closes
/// the listener, so a new connection is refused, and lets the requests in
/// flight be answered for at most [`DRAIN`]. Every stream ends as soon as its
/// page reads again.
///
/// A connection is served only while one of [`MAX_CONNECTIONS`] permits is
/// free, and a stop is heard while the provider waits for one. The provider
/// speaks HTTP/1.1, which is what `tailscale serve` forwards to it, and the
/// timer makes hyper's header-read timeout apply from the first poll, so a
/// peer that opens a connection and sends no request, or sends one slowly,
/// loses it rather than holding it open.
pub(crate) async fn until_stopped(
    listener: TcpListener,
    web: Arc<Web>,
    asked_to_stop: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let stopping = web.stopping.clone();
    let service = router(web);
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let graceful = GracefulShutdown::new();
    let mut asked_to_stop = pin!(asked_to_stop);
    loop {
        let next = async {
            let permit = Arc::clone(&connections).acquire_owned().await;
            let accepted = listener.accept().await;
            if let Err(error) = &accepted {
                warn!(
                    error = format!("{error:#}"),
                    "accepting a connection failed"
                );
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
            (permit, accepted)
        };
        let (permit, stream) = tokio::select! {
            (permit, accepted) = next => match (permit, accepted) {
                (Ok(permit), Ok((stream, _))) => (permit, stream),
                (Err(_), _) => break,
                (Ok(_), Err(_)) => continue,
            },
            () = &mut asked_to_stop => break,
        };
        let service = service.clone();
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .max_buf_size(READ_BUFFER)
            .max_header_size(READ_BUFFER);
        let connection = builder.serve_connection(
            TokioIo::new(stream),
            service_fn(move |request: hyper::Request<Incoming>| {
                service.clone().oneshot(request.map(Body::new))
            }),
        );
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            let _permit = permit;
            // A browser that stops reading, or stops sending, holds its
            // connection open until the graceful shutdown's bound or a
            // timeout. A message it was sending is not lost: the page sends
            // it again under the same event ID.
            let _ = connection.await;
        });
    }
    drop(listener);
    stopping.cancel();
    tokio::select! {
        () = graceful.shutdown() => Ok(()),
        () = tokio::time::sleep(DRAIN) => {
            warn!("connections were still open {DRAIN:?} after the provider was told to stop");
            Ok(())
        }
    }
}

/// Every route. The gate and [`BODY_IDLE_TIMEOUT`] wrap them all, the
/// fallback included, so a route added here cannot be reached around them. A
/// route added under `/api` needs the browser's token and one under `/v1` the
/// operator's credential, whether or not it remembers to ask.
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
        // Anyone who can reach the provider can send a guess, and a guess is
        // read whole before it is checked, so its body is kept small and its
        // time short.
        .route(
            "/api/sign-in",
            post(api::sign_in).layer((
                DefaultBodyLimit::max(4096),
                TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, SIGN_IN_TIMEOUT),
            )),
        )
        .nest("/api", api)
        .nest("/v1", operator)
        .route("/files/{*path}", get(files::file))
        .fallback(site::file)
        .layer(from_fn_with_state(Arc::clone(&web), gate))
        .layer(RequestBodyTimeoutLayer::new(BODY_IDLE_TIMEOUT))
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
    let key = Key::read(&web.browser.password_file);
    let Some(key) = key.filter(|key| key.authenticates(bearer(request.headers()))) else {
        return Err(refusal(
            StatusCode::UNAUTHORIZED,
            "This browser is not signed in. Sign in with the web chat's password.",
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
