//! The provider's routes, driven as a browser, the operator and a hostile
//! page would drive them.

use std::{
    net::TcpListener,
    os::unix::fs::{PermissionsExt as _, symlink},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{
        Method, Request, StatusCode,
        header::{AUTHORIZATION, HOST, ORIGIN},
    },
};
use carillon_installation::config::LoadedConfig;
use hmac::{Hmac, Mac as _};
use serde_json::json;
use sha2::Sha256;

use crate::{
    browser::{Key, now},
    fixture::{
        AUTHORITY, Fixture, INTERFACE_TOKEN, OPERATOR_TOKEN, bearer, operator, request, trade,
    },
};

/// One route of each class the provider serves.
const ROUTES: [(Method, &str); 12] = [
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

    let key_file = fixture.key_file();
    let before = std::fs::read_to_string(&key_file)?;
    assert!(!before.contains(&token), "the token is not the key");
    // What `carillon web sign-out` does. The provider is not told: the
    // very next request reads the new key.
    Key::replace(&key_file)?;
    assert_ne!(std::fs::read_to_string(&key_file)?, before);
    assert_eq!(
        std::fs::metadata(&key_file)?.permissions().mode() & 0o777,
        0o600
    );
    let after = fixture.send(session(Some(&token))?).await?;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    let again = fixture.sign_in().await?;
    assert_ne!(again, token);

    // Without a key, or with an empty one, nobody is signed in: not even
    // with the token anyone can make from no key at all.
    let mut keyless = Hmac::<Sha256>::new_from_slice(b"")?;
    keyless.update(&5_u64.to_be_bytes());
    keyless.update(b"token");
    let keyless = hex::encode(keyless.finalize().into_bytes());
    for no_key in [Some(""), Some("\n"), None] {
        match no_key {
            Some(empty) => std::fs::write(&key_file, empty)?,
            None => std::fs::remove_file(&key_file)?,
        }
        for token in [again.as_str(), keyless.as_str()] {
            let answer = fixture.send(session(Some(token))?).await?;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{no_key:?}");
        }
    }
    Ok(())
}

#[test]
fn signing_out_needs_no_provider_running() -> Result<()> {
    let installation = tempfile::tempdir()?;
    let key_file = crate::browser::key_file(installation.path(), "web");
    std::fs::create_dir_all(installation.path().join("components/web"))?;
    let first = Key::replace(&key_file)?.token();
    assert_eq!(
        std::fs::metadata(&key_file)?.permissions().mode() & 0o777,
        0o600
    );
    assert_ne!(Key::replace(&key_file)?.token(), first);
    Ok(())
}

/// What a trade of `code` is answered with, which names the way back in
/// when it is a refusal.
async fn traded(fixture: &Fixture, code: String) -> Result<StatusCode> {
    let answer = fixture.send(trade(&code)?).await?;
    if answer.status != StatusCode::OK {
        assert!(
            answer.json()?["error"]
                .as_str()
                .is_some_and(|said| said.contains("carillon web open"))
        );
    }
    Ok(answer.status)
}

#[tokio::test]
async fn a_sign_in_code_works_once_until_it_expires_under_the_key_that_signed_it() -> Result<()> {
    let fixture = Fixture::new().await?;
    let status = |code: String| traded(&fixture, code);
    let code = fixture.code(now() + 120)?;
    assert_eq!(status(code.clone()).await?, StatusCode::OK);
    assert_eq!(status(code.clone()).await?, StatusCode::UNAUTHORIZED);
    // The same code in other letters is the same code.
    let (signed, signature) = code.rsplit_once('.').unwrap_or_default();
    let respelled = format!("{signed}.{}", signature.to_uppercase());
    assert_eq!(status(respelled).await?, StatusCode::UNAUTHORIZED);

    // Past its expiry, though the key signed it.
    for past in [now() - 1, 1] {
        let expired = fixture.code(past)?;
        assert_eq!(status(expired).await?, StatusCode::UNAUTHORIZED);
    }
    // Not what the key signed.
    let good = fixture.code(now() + 120)?;
    let (expires, rest) = good.split_once('.').unwrap_or_default();
    let (name, signature) = rest.split_once('.').unwrap_or_default();
    for forged in [
        format!("{}.{name}.{signature}", now() + 3600),
        format!("{expires}.other{name}.{signature}"),
        format!("{expires}.{name}.{}", "0".repeat(signature.len())),
        format!("{expires}.{name}."),
        format!("{expires}.{name}"),
        format!("{good}.more"),
        "never-issued".to_owned(),
        String::new(),
    ] {
        assert_eq!(
            status(forged.clone()).await?,
            StatusCode::UNAUTHORIZED,
            "{forged}"
        );
    }
    // None of those spent it.
    assert_eq!(status(good).await?, StatusCode::OK);

    // Signed by a key since replaced.
    let waiting = fixture.code(now() + 120)?;
    Key::replace(&fixture.key_file())?;
    assert_eq!(status(waiting).await?, StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_requests_racing_for_one_code_sign_one_browser_in() -> Result<()> {
    let fixture = Arc::new(Fixture::new().await?);
    for _ in 0..20 {
        let code = fixture.code(now() + 120)?;
        let racers = [(); 2].map(|()| {
            let (fixture, request) = (Arc::clone(&fixture), trade(&code));
            tokio::spawn(async move { anyhow::Ok(fixture.send(request?).await?.status) })
        });
        let mut statuses = Vec::new();
        for racer in racers {
            statuses.push(racer.await??);
        }
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::UNAUTHORIZED]);
    }
    Ok(())
}

#[tokio::test]
async fn an_operator_route_takes_only_the_operator_credential() -> Result<()> {
    let fixture = Fixture::new().await?;
    let browser = fixture.sign_in().await?;
    for path in ["/v1/channels", "/v1/channels/300/settings", "/v1/unknown"] {
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
    // `carillon settings` is told that a web chat has no page of its own,
    // and nothing the operator's credential reaches signs a browser in or out.
    let listed = fixture
        .send(operator(Method::GET, "/v1/channels").body(Body::empty())?)
        .await?;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.json()?, json!([]));
    for (method, path) in [
        (
            Method::GET,
            "/v1/channels/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/settings",
        ),
        (Method::PUT, "/v1/channels/300/settings"),
        (Method::POST, "/v1/sign-in"),
        (Method::POST, "/v1/sign-out"),
    ] {
        let answer = fixture
            .send(operator(method, path).body(Body::empty())?)
            .await?;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert!(fixture.core().asked.is_empty(), "core was asked");
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

#[tokio::test]
async fn only_a_file_that_is_itself_in_the_page_s_folder_is_served() -> Result<()> {
    let fixture = Fixture::new().await?;
    let root = fixture.directory.path();
    let page = |path: &'static str| async {
        fixture
            .send(request(Method::GET, path).body(Body::empty())?)
            .await
    };
    std::fs::write(root.join("secret.token"), "operator-secret")?;
    std::fs::create_dir(root.join("release/folder.js"))?;
    let pipe = std::process::Command::new("mkfifo")
        .arg(root.join("release/pipe.js"))
        .status()?;
    assert!(pipe.success());
    let custom = root.join("state/components/web/site");
    std::fs::create_dir_all(&custom)?;
    // A page that is a link to one is no copy of the page.
    symlink(root.join("release/index.html"), custom.join("index.html"))?;
    for folder in [root.join("release"), custom.clone()] {
        symlink(root.join("secret.token"), folder.join("leak.js"))?;
        symlink(root.join("release/app.js"), folder.join("linked.js"))?;
        symlink("app.js", folder.join("beside.js"))?;
    }
    let refused = [
        "/leak.js",
        "/linked.js",
        "/beside.js",
        "/folder.js",
        "/pipe.js",
    ];
    for path in refused {
        let answer = page(path).await?;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{path}");
        assert!(answer.body.is_empty(), "{path} was read");
    }
    assert_eq!(page("/").await?.body, b"<main>release page</main>");

    std::fs::remove_file(custom.join("index.html"))?;
    std::fs::write(custom.join("index.html"), "<main>custom page</main>")?;
    assert_eq!(page("/").await?.body, b"<main>custom page</main>");
    for path in &refused[..3] {
        assert_eq!(page(path).await?.status, StatusCode::NOT_FOUND, "{path}");
    }
    Ok(())
}

#[tokio::test]
async fn open_makes_a_link_the_provider_takes_and_says_when_none_runs() -> Result<()> {
    let fixture = Fixture::new().await?;
    // The same installation, with the provider at an address that is this
    // test's alone.
    let listening = TcpListener::bind("127.0.0.1:0")?;
    let address = listening.local_addr()?.to_string();
    let root = fixture.directory.path();
    let registry = std::fs::read_to_string(root.join("carillon.toml"))?;
    std::fs::write(
        root.join("elsewhere.toml"),
        registry.replace(AUTHORITY, &address),
    )?;
    let loaded = LoadedConfig::load(&root.join("elsewhere.toml"))?;

    let made = crate::link(&loaded, "web")?;
    let code = made
        .strip_prefix(&format!("http://{address}/#sign-in="))
        .with_context(|| format!("{made} does not open the provider's page"))?;
    let expires: u64 = code.split('.').next().unwrap_or_default().parse()?;
    assert!(
        (now() + 115..=now() + 120).contains(&expires),
        "the link does not work for two minutes"
    );
    // Nothing was asked of the listener, and the code is good all the same.
    listening.set_nonblocking(true)?;
    let (probe, _) = listening.accept()?;
    let mut sent = [0_u8; 1];
    assert_eq!(std::io::Read::read(&mut &probe, &mut sent)?, 0);
    assert_eq!(fixture.send(trade(code)?).await?.status, StatusCode::OK);

    drop((probe, listening));
    let stopped = crate::link(&loaded, "web")
        .err()
        .context("a link to nothing")?;
    assert!(
        stopped
            .to_string()
            .contains(&format!("is not running at http://{address}")),
        "{stopped}"
    );
    Ok(())
}
