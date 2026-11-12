//! The provider's routes, driven as a browser, the operator and a hostile
//! page would drive them.

use std::{os::unix::fs::PermissionsExt as _, time::Duration};

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{
        Method, Request, StatusCode,
        header::{AUTHORIZATION, HOST, ORIGIN},
    },
};
use serde_json::json;

use crate::fixture::{
    AUTHORITY, Fixture, INTERFACE_TOKEN, OPERATOR_TOKEN, bearer, operator, request, trade,
};

/// One route of each class the provider serves.
const ROUTES: [(Method, &str); 13] = [
    (Method::GET, "/"),
    (Method::GET, "/app.js"),
    (Method::GET, "/health"),
    (Method::POST, "/api/sign-in"),
    (Method::GET, "/api/session"),
    (Method::GET, "/api/conversations"),
    (
        Method::PUT,
        "/api/conversations/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/messages/m-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b",
    ),
    (Method::GET, "/api/turns?ids=a"),
    (
        Method::POST,
        "/api/messages/m-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/control",
    ),
    (Method::POST, "/v1/sign-in"),
    (Method::GET, "/v1/channels"),
    (Method::PUT, "/v1/channels/300/settings"),
    (Method::GET, "/files/capture/0?name=a&exp=1&sig=00"),
];

#[tokio::test]
async fn a_request_that_does_not_name_this_provider_is_misdirected() -> Result<()> {
    let fixture = Fixture::new().await?;
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
    let fixture = Fixture::new().await?;
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
    let fixture = Fixture::new().await?;
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
                "files": 3,
                "file_bytes": 1024 * 1024,
                "message_bytes": 2 * 1024 * 1024,
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
    let fixture = Fixture::new().await?;
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
    let fixture = Fixture::new().await?;
    let browser = fixture.sign_in().await?;
    for path in [
        "/v1/sign-in",
        "/v1/sign-out",
        "/v1/channels",
        "/v1/channels/300/settings",
        "/v1/unknown",
    ] {
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
    let fixture = Fixture::new().await?;
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
    let fixture = Fixture::new().await?;
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
