//! The provider's routes, driven as a browser, the operator and a hostile
//! page would drive them.

use std::{os::unix::fs::PermissionsExt as _, sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use axum::{
    body::{Body, to_bytes},
    http::{
        Method, Request, StatusCode,
        header::{AUTHORIZATION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, ORIGIN},
        request::Builder,
        response::Parts,
    },
};
use carillon_installation::config::{LoadedConfig, hash_token};
use carillon_protocol::model::Health;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

use super::{Web, router};
use crate::{
    browser::{self, Browser},
    site::Site,
};

pub(crate) const AUTHORITY: &str = "127.0.0.1:45404";
const INTERFACE_TOKEN: &str = "interface-token";
const OPERATOR_TOKEN: &str = "operator-token";

pub(crate) struct Fixture {
    pub(crate) directory: TempDir,
    pub(crate) web: Arc<Web>,
}

/// An answer, read whole.
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) parts: Parts,
    pub(crate) body: Vec<u8>,
}

impl Answer {
    pub(crate) fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.body)
            .with_context(|| format!("{}: {}", self.status, String::from_utf8_lossy(&self.body)))
    }

    pub(crate) fn header(&self, name: &str) -> &str {
        self.parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }
}

impl Fixture {
    pub(crate) fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let registry = format!(
            r#"data_dir = "state"
[core]
listen = "127.0.0.1:45400"
operator_token_hash = "{operator}"
[[providers]]
id = "web"
role = "interface"
program = "web"
version = "0.0.0"
url = "http://{AUTHORITY}"
token_hash = "{token}"
enabled = true
max_upload_mib = 1024
delivery = "retained"
[[providers]]
id = "runtime-other"
role = "runtime"
program = "runtime-other"
version = "0.0.0"
url = "http://127.0.0.1:45402"
token_hash = "{token}"
enabled = true
[[agents]]
id = "assistant"
name = "Assistant"
runtime = "runtime-other"
workspace = "work"
[[agents]]
id = "second"
name = "Second"
runtime = "runtime-other"
workspace = "work"
"#,
            operator = hash_token(OPERATOR_TOKEN),
            token = hash_token(INTERFACE_TOKEN),
        );
        std::fs::write(root.join("carillon.toml"), registry)?;
        let release = root.join("release");
        std::fs::create_dir_all(&release)?;
        std::fs::write(release.join("index.html"), "<main>release page</main>")?;
        std::fs::write(release.join("app.js"), "// release script")?;
        let registry = LoadedConfig::load(&root.join("carillon.toml"))?;
        let data_dir = &registry.value.data_dir;
        let web = Arc::new(Web {
            health: Health {
                id: "web".to_owned(),
                role: "interface".to_owned(),
                version: "0.0.0".to_owned(),
                status: "ready".to_owned(),
            },
            browser: Browser::open(browser::key_file(data_dir, "web"), AUTHORITY.to_owned())?,
            site: Site {
                custom: data_dir.join("components/web/site"),
                release,
            },
            registry,
        });
        Ok(Self { directory, web })
    }

    /// The answer to `request`. No answer of this provider may carry a CORS
    /// header, and each has the headers that keep it on its origin.
    pub(crate) async fn send(&self, request: Request<Body>) -> Result<Answer> {
        let response = router(Arc::clone(&self.web)).oneshot(request).await?;
        let (parts, body) = response.into_parts();
        for name in parts.headers.keys() {
            assert!(
                !name.as_str().starts_with("access-control-"),
                "{name} was sent"
            );
        }
        assert_eq!(parts.headers["x-content-type-options"], "nosniff");
        assert_eq!(parts.headers["referrer-policy"], "no-referrer");
        assert_eq!(parts.headers["cross-origin-resource-policy"], "same-origin");
        assert!(parts.headers.contains_key(CONTENT_SECURITY_POLICY));
        assert!(!parts.headers.contains_key("set-cookie"));
        Ok(Answer {
            status: parts.status,
            parts,
            body: to_bytes(body, usize::MAX).await?.to_vec(),
        })
    }

    /// A token as a browser gets one: a code from the operator's route,
    /// traded at the page's.
    pub(crate) async fn sign_in(&self) -> Result<String> {
        let issued = self
            .send(operator(Method::POST, "/v1/sign-in").body(Body::empty())?)
            .await?;
        assert_eq!(issued.status, StatusCode::OK);
        let code = issued.json()?["code"]
            .as_str()
            .context("no code")?
            .to_owned();
        let traded = self.send(trade(&code)?).await?;
        assert_eq!(traded.status, StatusCode::OK);
        Ok(traded.json()?["token"]
            .as_str()
            .context("no token")?
            .to_owned())
    }
}

/// A request that names this provider, as a program sends one.
pub(crate) fn request(method: Method, path: &str) -> Builder {
    Request::builder()
        .method(method)
        .uri(path)
        .header(HOST, AUTHORITY)
}

pub(crate) fn bearer(method: Method, path: &str, token: &str) -> Builder {
    request(method, path).header(AUTHORIZATION, format!("Bearer {token}"))
}

pub(crate) fn operator(method: Method, path: &str) -> Builder {
    bearer(method, path, OPERATOR_TOKEN)
}

fn trade(code: &str) -> Result<Request<Body>> {
    Ok(request(Method::POST, "/api/sign-in")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "code": code }).to_string()))?)
}

/// One route of each class the provider serves.
const ROUTES: [(Method, &str); 6] = [
    (Method::GET, "/"),
    (Method::GET, "/app.js"),
    (Method::GET, "/health"),
    (Method::POST, "/api/sign-in"),
    (Method::GET, "/api/session"),
    (Method::POST, "/v1/sign-in"),
];

#[tokio::test]
async fn a_request_that_does_not_name_this_provider_is_misdirected() -> Result<()> {
    let fixture = Fixture::new()?;
    for (method, path) in ROUTES {
        let named = |hosts: &[&str]| {
            let mut request = Request::builder().method(method.clone()).uri(path);
            for host in hosts {
                request = request.header(HOST, *host);
            }
            request
                .header(AUTHORIZATION, format!("Bearer {OPERATOR_TOKEN}"))
                .body(Body::empty())
        };
        for hosts in [
            &[][..],
            &["rebound.example"],
            &["rebound.example:45404"],
            &["localhost:45404"],
            &["127.0.0.1"],
            &[AUTHORITY, AUTHORITY],
            &[AUTHORITY, "rebound.example"],
        ] {
            let answer = fixture.send(named(hosts)?).await?;
            assert_eq!(
                answer.status,
                StatusCode::MISDIRECTED_REQUEST,
                "{method} {path} as {hosts:?}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn another_origin_and_every_preflight_are_refused() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.sign_in().await?;
    for (method, path) in ROUTES {
        for origin in [
            "http://evil.example",
            "null",
            "https://127.0.0.1:45404",
            "http://127.0.0.1:45405",
            "http://localhost:45404",
        ] {
            let answer = fixture
                .send(
                    bearer(method.clone(), path, &token)
                        .header(ORIGIN, origin)
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(
                answer.status,
                StatusCode::FORBIDDEN,
                "{method} {path} from {origin}"
            );
        }
        for origin in ["http://evil.example", "http://127.0.0.1:45404"] {
            let preflight = bearer(Method::OPTIONS, path, &token)
                .header(ORIGIN, origin)
                .header("access-control-request-method", method.as_str())
                .header("access-control-request-headers", "authorization")
                .body(Body::empty())?;
            let answer = fixture.send(preflight).await?;
            assert!(
                answer.status.is_client_error(),
                "{path} answered a preflight from {origin} with {}",
                answer.status
            );
        }
    }
    let own = bearer(Method::GET, "/api/session", &token)
        .header(ORIGIN, "http://127.0.0.1:45404")
        .body(Body::empty())?;
    assert_eq!(fixture.send(own).await?.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn the_api_needs_the_token_the_current_key_gives() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.sign_in().await?;
    let session = |token: Option<&str>| {
        match token {
            Some(token) => bearer(Method::GET, "/api/session", token),
            None => request(Method::GET, "/api/session"),
        }
        .body(Body::empty())
    };
    let signed_in = fixture.send(session(Some(&token))?).await?;
    assert_eq!(signed_in.status, StatusCode::OK);
    assert_eq!(
        signed_in.json()?,
        json!({
            "agents": [
                {"id": "assistant", "name": "Assistant"},
                {"id": "second", "name": "Second"},
            ],
            "limits": {
                "files": 10,
                "file_bytes": 1024 * 1024 * 1024_u64,
                "message_bytes": 2048 * 1024 * 1024_u64,
            },
            "custom_site": false,
        })
    );
    for refused in [
        None,
        Some("wrong"),
        Some(""),
        Some(INTERFACE_TOKEN),
        Some(OPERATOR_TOKEN),
    ] {
        let answer = fixture.send(session(refused)?).await?;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{refused:?}");
    }
    // A path under `/api` that is no route asks for the token all the same.
    let unknown = fixture
        .send(request(Method::GET, "/api/unknown").body(Body::empty())?)
        .await?;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

    let key_file = fixture
        .directory
        .path()
        .join("state/components/web/browser.key");
    let before = std::fs::read_to_string(&key_file)?;
    assert_eq!(
        std::fs::metadata(&key_file)?.permissions().mode() & 0o777,
        0o600
    );
    assert!(!before.contains(&token), "the token is not the key");
    let signed_out = fixture
        .send(operator(Method::POST, "/v1/sign-out").body(Body::empty())?)
        .await?;
    assert_eq!(signed_out.status, StatusCode::NO_CONTENT);
    assert_ne!(std::fs::read_to_string(&key_file)?, before);
    assert_eq!(
        std::fs::metadata(&key_file)?.permissions().mode() & 0o777,
        0o600
    );
    let after = fixture.send(session(Some(&token))?).await?;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    assert_ne!(fixture.sign_in().await?, token);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_sign_in_code_works_once_and_for_two_minutes() -> Result<()> {
    let fixture = Fixture::new()?;
    let issue = || async {
        let issued = fixture
            .send(operator(Method::POST, "/v1/sign-in").body(Body::empty())?)
            .await?;
        anyhow::Ok(
            issued.json()?["code"]
                .as_str()
                .context("no code")?
                .to_owned(),
        )
    };
    let code = issue().await?;
    assert_eq!(fixture.send(trade(&code)?).await?.status, StatusCode::OK);
    let again = fixture.send(trade(&code)?).await?;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
    assert!(
        again.json()?["error"]
            .as_str()
            .is_some_and(|said| said.contains("carillon web open"))
    );
    assert_eq!(
        fixture.send(trade("never-issued")?).await?.status,
        StatusCode::UNAUTHORIZED
    );

    let late = issue().await?;
    let kept = issue().await?;
    tokio::time::advance(Duration::from_secs(119)).await;
    assert_eq!(fixture.send(trade(&kept)?).await?.status, StatusCode::OK);
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(
        fixture.send(trade(&late)?).await?.status,
        StatusCode::UNAUTHORIZED
    );

    // Signing out voids a code not yet used.
    let waiting = issue().await?;
    fixture
        .send(operator(Method::POST, "/v1/sign-out").body(Body::empty())?)
        .await?;
    assert_eq!(
        fixture.send(trade(&waiting)?).await?.status,
        StatusCode::UNAUTHORIZED
    );
    Ok(())
}

#[tokio::test]
async fn an_operator_route_takes_only_the_operator_credential() -> Result<()> {
    let fixture = Fixture::new()?;
    let browser = fixture.sign_in().await?;
    for path in ["/v1/sign-in", "/v1/sign-out", "/v1/unknown"] {
        for refused in [
            None,
            Some(INTERFACE_TOKEN),
            Some(browser.as_str()),
            Some(""),
        ] {
            let request = match refused {
                Some(token) => bearer(Method::POST, path, token),
                None => request(Method::POST, path),
            };
            let answer = fixture.send(request.body(Body::empty())?).await?;
            assert_eq!(
                answer.status,
                StatusCode::FORBIDDEN,
                "{path} with {refused:?}"
            );
        }
    }
    // None of those refusals signed the browser out.
    let session = bearer(Method::GET, "/api/session", &browser).body(Body::empty())?;
    assert_eq!(fixture.send(session).await?.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn health_and_the_page_need_no_credential() -> Result<()> {
    let fixture = Fixture::new()?;
    let health = fixture
        .send(request(Method::GET, "/health").body(Body::empty())?)
        .await?;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(
        health.json()?,
        json!({"id": "web", "role": "interface", "version": "0.0.0", "status": "ready"})
    );

    let page = |path: &'static str| async {
        fixture
            .send(request(Method::GET, path).body(Body::empty())?)
            .await
    };
    let index = page("/").await?;
    assert_eq!(index.status, StatusCode::OK);
    assert_eq!(index.body, b"<main>release page</main>");
    assert_eq!(index.header("content-type"), "text/html; charset=utf-8");
    let policy = index.header("content-security-policy");
    assert!(policy.contains("default-src 'none'") && policy.contains("script-src 'self'"));
    assert!(!policy.contains("unsafe"));
    assert_eq!(index.header("cache-control"), "no-store");
    let script = page("/app.js").await?;
    assert_eq!(
        script.header("content-type"),
        "text/javascript; charset=utf-8"
    );
    for absent in [
        "/missing.js",
        "/index",
        "/../carillon.toml",
        "/%2e%2e/carillon.toml",
        "/.hidden.js",
        "/a/app.js",
        "//app.js",
    ] {
        assert_eq!(
            page(absent).await?.status,
            StatusCode::NOT_FOUND,
            "{absent}"
        );
    }
    let posted = fixture
        .send(request(Method::POST, "/").body(Body::empty())?)
        .await?;
    assert_eq!(posted.status, StatusCode::METHOD_NOT_ALLOWED);
    Ok(())
}

#[tokio::test]
async fn a_complete_copy_of_the_page_in_the_provider_s_state_is_served_instead() -> Result<()> {
    let fixture = Fixture::new()?;
    let token = fixture.sign_in().await?;
    let custom = fixture.directory.path().join("state/components/web/site");
    std::fs::create_dir_all(&custom)?;
    // A directory without a page of its own is not a copy.
    std::fs::write(custom.join("app.js"), "// custom script")?;
    let script = || async {
        fixture
            .send(request(Method::GET, "/app.js").body(Body::empty())?)
            .await
    };
    let custom_site = || async {
        let session = bearer(Method::GET, "/api/session", &token).body(Body::empty())?;
        anyhow::Ok(fixture.send(session).await?.json()?["custom_site"].clone())
    };
    assert_eq!(script().await?.body, b"// release script");
    assert_eq!(custom_site().await?, false);
    std::fs::write(custom.join("index.html"), "<main>custom page</main>")?;
    assert_eq!(script().await?.body, b"// custom script");
    assert_eq!(custom_site().await?, true);
    // The copy is served whole: a file it lacks does not come from the release.
    std::fs::remove_file(custom.join("app.js"))?;
    assert_eq!(script().await?.status, StatusCode::NOT_FOUND);
    Ok(())
}
