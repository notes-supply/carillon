//! The provider's routes, driven as a browser, the operator and a hostile
//! page would drive them.

use std::{
    os::unix::fs::{PermissionsExt as _, symlink},
    sync::Arc,
    time::Duration,
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
use tokio::time::Instant;

use crate::{
    browser::{Address, addresses, password_file, set_password},
    fixture::{
        AUTHORITY, Fixture, INTERFACE_TOKEN, OPERATOR_TOKEN, PASSWORD, TAILNET, bearer, guess,
        operator, request,
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
            &["other.example-tailnet.ts.net"],
            &["chat.example-tailnet.ts.net:443"],
        ] {
            let answer = fixture.send(named(hosts)?).await?;
            assert_eq!(
                answer.status,
                StatusCode::MISDIRECTED_REQUEST,
                "{method} {path} as {hosts:?}"
            );
        }
    }
    let rebound = Request::builder()
        .uri("/")
        .header(HOST, "rebound.example")
        .body(Body::empty())?;
    let answer = fixture.send(rebound).await?;
    assert_eq!(
        answer.json()?["error"],
        "This is Carillon's web chat at http://127.0.0.1:45404. To open it at another address, \
         add that address to `addresses` in providers/web.toml."
    );
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
            "http://chat.example-tailnet.ts.net",
            "https://chat.example-tailnet.ts.net:8443",
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
async fn the_page_answers_at_an_address_the_owner_named() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    // A browser's request as `tailscale serve` forwards it, Host and all.
    let named = |path: &str| {
        Request::builder()
            .uri(path)
            .header(HOST, "chat.example-tailnet.ts.net")
            .header(ORIGIN, TAILNET)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
    };
    for path in ["/", "/api/session"] {
        assert_eq!(
            fixture.send(named(path)?).await?.status,
            StatusCode::OK,
            "{path}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn the_api_needs_the_token_the_current_password_gives() -> Result<()> {
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

    let file = fixture.password_file();
    let before = std::fs::read_to_string(&file)?;
    assert!(!before.contains(&token), "the token is not the hash");
    // Choosing the same password again makes a new hash, and signs out.
    set_password(&file, PASSWORD)?;
    assert_ne!(std::fs::read_to_string(&file)?, before);
    let after = fixture.send(session(Some(&token))?).await?;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    let again = fixture.sign_in().await?;
    assert_ne!(again, token);

    let mut keyless = Hmac::<Sha256>::new_from_slice(b"")?;
    keyless.update(&5_u64.to_be_bytes());
    keyless.update(b"token");
    let keyless = hex::encode(keyless.finalize().into_bytes());
    for no_key in [Some(""), Some("\n"), None] {
        match no_key {
            Some(empty) => std::fs::write(&file, empty)?,
            None => std::fs::remove_file(&file)?,
        }
        for token in [again.as_str(), keyless.as_str()] {
            let answer = fixture.send(session(Some(token))?).await?;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{no_key:?}");
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_password_alone_signs_a_browser_in() -> Result<()> {
    let fixture = Fixture::new().await?;
    let file = fixture.password_file();
    // Space around a password is no part of it, where it is set or typed.
    set_password(&file, "\tcorrect horse battery \n")?;
    for typed in [PASSWORD, "  correct horse battery\n"] {
        let answer = fixture.send(guess(typed)?).await?;
        assert_eq!(answer.status, StatusCode::OK, "{typed:?}");
        let token = answer.json()?["token"]
            .as_str()
            .context("no token")?
            .to_owned();
        let session = bearer(Method::GET, "/api/session", &token).body(Body::empty())?;
        assert_eq!(fixture.send(session).await?.status, StatusCode::OK);
    }
    for wrong in ["correct horse batter", "Correct horse battery", ""] {
        let answer = fixture.send(guess(wrong)?).await?;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{wrong:?}");
        assert_eq!(
            answer.json()?["error"],
            "That is not the web chat's password."
        );
    }
    std::fs::remove_file(&file)?;
    let unset = fixture.send(guess(PASSWORD)?).await?;
    assert_eq!(unset.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        unset.json()?["error"],
        "The web chat has no password yet. Run `carillon web password` in a terminal to choose \
         one."
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn guesses_wait_their_turn_and_a_wrong_one_holds_it_a_second() -> Result<()> {
    let fixture = Arc::new(Fixture::new().await?);
    let sent = |password: &'static str| {
        let fixture = Arc::clone(&fixture);
        tokio::spawn(async move {
            let answer = fixture.send(guess(password)?).await?;
            anyhow::Ok((answer.status, Instant::now()))
        })
    };
    let start = Instant::now();
    let wrong = sent("not the password at all");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let right = sent(PASSWORD);
    let (wrong, wrong_at) = wrong.await??;
    let (right, right_at) = right.await??;
    assert_eq!((wrong, right), (StatusCode::UNAUTHORIZED, StatusCode::OK));
    assert!(wrong_at - start >= Duration::from_secs(1));
    assert!(
        right_at >= wrong_at,
        "a guess finished during a wrong one's second"
    );
    Ok(())
}

#[test]
fn a_password_is_set_before_the_provider_first_starts() -> Result<()> {
    let installation = tempfile::tempdir()?;
    let file = password_file(installation.path(), "web");
    let mode =
        |path: &std::path::Path| anyhow::Ok(std::fs::metadata(path)?.permissions().mode() & 0o777);
    for short in ["", "elevenchars", "   short and   \n"] {
        let refused = set_password(&file, short)
            .err()
            .context("a short password")?;
        assert!(refused.to_string().contains("12 characters"), "{refused}");
        assert!(
            !installation.path().join("components").exists(),
            "{short:?}"
        );
    }
    set_password(&file, "twelve chars")?;
    let hash = std::fs::read_to_string(&file)?;
    assert!(hash.starts_with("$argon2id$v=19$"), "{hash}");
    assert_eq!(mode(&file)?, 0o600);
    assert_eq!(mode(file.parent().context("a directory")?)?, 0o700);
    assert!(set_password(&file, "too short").err().is_some());
    assert_eq!(std::fs::read_to_string(&file)?, hash);
    Ok(())
}

#[tokio::test]
async fn the_owner_s_addresses_are_read_as_a_browser_writes_them() -> Result<()> {
    for (written, host, origin) in [
        (TAILNET, "chat.example-tailnet.ts.net", TAILNET),
        (
            "https://Chat.Example-Tailnet.TS.net:443/",
            "chat.example-tailnet.ts.net",
            TAILNET,
        ),
        (
            "http://192.168.1.4:8080",
            "192.168.1.4:8080",
            "http://192.168.1.4:8080",
        ),
        ("http://[::1]:80", "[::1]", "http://[::1]"),
    ] {
        let expected = Address {
            host: host.to_owned(),
            origin: origin.to_owned(),
        };
        assert_eq!(Address::parse(written)?, expected, "{written}");
    }
    for refused in [
        "chat.example-tailnet.ts.net",
        "localhost:45404",
        "ftp://chat.example",
        "https://owner@chat.example",
        "https://chat.example/chat",
        "https://chat.example/?page",
        "https://chat.example/#page",
    ] {
        let said = Address::parse(refused).err().context(refused)?.to_string();
        assert!(
            said.starts_with(&format!("{refused} is not an address")),
            "{said}"
        );
    }

    let fixture = Fixture::new().await?;
    let registry: &LoadedConfig = &fixture.web.registry;
    assert!(addresses(registry, "web")?.is_empty());
    let settings = registry.provider_config_path("web")?;
    std::fs::create_dir_all(settings.parent().context("a directory")?)?;
    std::fs::write(&settings, format!("addresses = [\"{TAILNET}/\"]\n"))?;
    assert_eq!(addresses(registry, "web")?, [Address::parse(TAILNET)?]);
    for wrong in [
        "addresses = [\"https://chat.example/chat\"]\n",
        "address = [\"https://chat.example\"]\n",
        "addresses = \"https://chat.example\"\n",
    ] {
        std::fs::write(&settings, wrong)?;
        let said = format!("{:#}", addresses(registry, "web").err().context(wrong)?);
        assert!(said.contains(&settings.display().to_string()), "{said}");
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
    std::fs::write(
        fixture.directory.path().join("release/face.woff2"),
        "a font",
    )?;
    assert_eq!(
        page("/face.woff2").await?.header("content-type"),
        "font/woff2"
    );
    assert!(policy.contains("font-src 'self'"));
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

    // The folder itself may be a link: the owner names where the page
    // lives, and that folder is the page.
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir(&elsewhere)?;
    std::fs::write(elsewhere.join("index.html"), "<main>linked page</main>")?;
    std::fs::remove_dir_all(&custom)?;
    symlink(&elsewhere, &custom)?;
    assert_eq!(page("/").await?.body, b"<main>linked page</main>");
    Ok(())
}
