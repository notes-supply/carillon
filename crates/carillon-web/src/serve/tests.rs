//! The provider's routes, driven as a browser, the operator and a hostile
//! page would drive them.

use std::{
    os::unix::fs::{PermissionsExt as _, mkfifo, symlink},
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
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::json;
use sha2::Sha256;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::Notify,
    time::Instant,
};

use super::{HEADER_READ_TIMEOUT, until_stopped};
use crate::{
    browser::{Address, addresses, password_file, set_password},
    fixture::{
        AUTHORITY, Fixture, INTERFACE_TOKEN, OPERATOR_TOKEN, PASSWORD, TAILNET, bearer, guess,
        operator, request,
    },
};

/// A signed-in browser's stream. Opening one asks core nothing, so it
/// answers 200 on the token alone.
const STREAM: &str = "/api/stream?conversations=1";

/// One route of each class the provider serves.
const ROUTES: [(Method, &str); 12] = [
    (Method::GET, "/"),
    (Method::GET, "/index-Bq3nR7xK.js"),
    (Method::GET, "/health"),
    (Method::POST, "/api/sign-in"),
    (Method::GET, STREAM),
    (
        Method::GET,
        "/api/conversations/assistant/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/commands",
    ),
    (
        Method::PUT,
        "/api/conversations/assistant/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/messages/m-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b",
    ),
    (
        Method::POST,
        "/api/messages/m-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/control",
    ),
    (Method::GET, "/v1/channels"),
    (Method::PUT, "/v1/channels/300/settings"),
    (Method::GET, "/files/capture/0?name=a&exp=1&sig=00"),
    (Method::GET, "/api/unknown"),
];

#[tokio::test]
async fn a_request_that_does_not_name_this_provider_is_misdirected() -> Result<()> {
    let fixture = Fixture::new().await?;
    let browser = fixture.sign_in().await?;
    for (method, path) in ROUTES {
        let credential = if path.starts_with("/v1") {
            OPERATOR_TOKEN
        } else {
            browser.as_str()
        };
        let named = |hosts: &[&str]| {
            let mut request = Request::builder().method(method.clone()).uri(path);
            for host in hosts {
                request = request.header(HOST, *host);
            }
            request
                .header(AUTHORIZATION, format!("Bearer {credential}"))
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
    assert!(fixture.core().asked.is_empty(), "core was asked");
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
    assert!(fixture.core().asked.is_empty(), "core was asked");
    let own = bearer(Method::GET, STREAM, &token)
        .header(ORIGIN, "http://127.0.0.1:45404")
        .body(Body::empty())?;
    assert_eq!(fixture.status(own).await?, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn the_page_answers_at_an_address_the_owner_named() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let named = |path: &str| {
        Request::builder()
            .uri(path)
            .header(HOST, "chat.example-tailnet.ts.net")
            .header(ORIGIN, TAILNET)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
    };
    for path in ["/", STREAM] {
        assert_eq!(
            fixture.status(named(path)?).await?,
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
    let stream = |token: Option<&str>| {
        match token {
            Some(token) => bearer(Method::GET, STREAM, token),
            None => request(Method::GET, STREAM),
        }
        .body(Body::empty())
    };
    assert_eq!(fixture.status(stream(Some(&token))?).await?, StatusCode::OK);
    for refused in [
        None,
        Some("wrong"),
        Some(""),
        Some(INTERFACE_TOKEN),
        Some(OPERATOR_TOKEN),
    ] {
        let answer = fixture.send(stream(refused)?).await?;
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
    set_password(&file, PASSWORD)?;
    assert_ne!(std::fs::read_to_string(&file)?, before);
    let after = fixture.send(stream(Some(&token))?).await?;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    let again = fixture.sign_in().await?;
    assert_ne!(again, token);

    // A file that holds no whole hash keys nothing, so no token made from
    // its guessable text gets in.
    let guessed = |text: &str| -> Result<String> {
        let mut mac = Hmac::<Sha256>::new_from_slice(text.trim().as_bytes())?;
        mac.update(&5_u64.to_be_bytes());
        mac.update(b"token");
        Ok(hex::encode(mac.finalize().into_bytes()))
    };
    for no_key in [
        Some(""),
        Some("\n"),
        Some("not-a-PHC-hash"),
        Some("$argon2id$v=19$m=19456,t=2,p=1$"),
        Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0"),
        None,
    ] {
        match no_key {
            Some(text) => std::fs::write(&file, text)?,
            None => std::fs::remove_file(&file)?,
        }
        for token in [again.clone(), guessed(no_key.unwrap_or_default())?] {
            let answer = fixture.send(stream(Some(&token))?).await?;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{no_key:?}");
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn the_password_alone_signs_a_browser_in() -> Result<()> {
    let fixture = Fixture::new().await?;
    let file = fixture.password_file();
    set_password(&file, "\tcorrect horse battery \n")?;
    for typed in [PASSWORD, "  correct horse battery\n"] {
        let answer = fixture.send(guess(typed)?).await?;
        assert_eq!(answer.status, StatusCode::OK, "{typed:?}");
        let token = answer.json()?["token"]
            .as_str()
            .context("no token")?
            .to_owned();
        let stream = bearer(Method::GET, STREAM, &token).body(Body::empty())?;
        assert_eq!(fixture.status(stream).await?, StatusCode::OK);
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
async fn a_wrong_guess_holds_the_slot_a_second_and_a_guess_meanwhile_is_refused() -> Result<()> {
    let fixture = Arc::new(Fixture::new().await?);
    let start = Instant::now();
    // A wrong guess takes the one slot and holds it through its one-second
    // cooldown, serializing the Argon2 check.
    let wrong = {
        let fixture = Arc::clone(&fixture);
        tokio::spawn(async move {
            let answer = fixture.send(guess("not the password at all")?).await?;
            anyhow::Ok((answer.status, Instant::now()))
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The owner's guess, arriving while the slot is held, is refused at once
    // rather than queued behind the cooldown.
    let refused = fixture.send(guess(PASSWORD)?).await?;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        refused.json()?["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Another sign-in"),
        "{}",
        refused.text()
    );
    let (wrong, wrong_at) = wrong.await??;
    assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    assert!(
        wrong_at - start >= Duration::from_secs(1),
        "the cooldown was dropped"
    );
    // Once the slot is free, a guess is checked again.
    let right = fixture.send(guess(PASSWORD)?).await?;
    assert_eq!(right.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_guess_is_small() -> Result<()> {
    let fixture = Fixture::new().await?;
    let long = fixture.send(guess(&"x".repeat(5000))?).await?;
    assert_eq!(long.status, StatusCode::PAYLOAD_TOO_LARGE);
    let right = fixture.send(guess(PASSWORD)?).await?;
    assert_eq!(right.status, StatusCode::OK);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_dropped_guess_keeps_the_slot_until_its_check_ends() -> Result<()> {
    let fixture = Arc::new(Fixture::new().await?);
    let start = Instant::now();
    // A wrong guess whose request is dropped still runs to the end of its
    // cooldown in its own task, keeping the slot.
    let dropped = {
        let fixture = Arc::clone(&fixture);
        tokio::spawn(async move { fixture.send(guess("not the password at all")?).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    dropped.abort();
    // While that check runs on, a guess is refused rather than let in early.
    let refused = fixture.send(guess(PASSWORD)?).await?;
    assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
    // Once the dropped guess's cooldown is over, the slot frees and a guess is
    // checked again.
    let right = loop {
        let answer = fixture.send(guess(PASSWORD)?).await?;
        if answer.status != StatusCode::TOO_MANY_REQUESTS {
            break answer;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(right.status, StatusCode::OK);
    assert!(
        start.elapsed() >= Duration::from_secs(1),
        "the slot freed before the dropped guess's cooldown ended"
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
    let refused = set_password(&file, "too short")
        .err()
        .context("a short password")?;
    assert!(refused.to_string().contains("12 characters"), "{refused}");
    assert_eq!(std::fs::read_to_string(&file)?, hash);
    Ok(())
}

#[test]
fn a_plain_http_address_is_refused_for_https() -> Result<()> {
    for written in [
        "http://chat.example-tailnet.ts.net",
        "http://chat.example-tailnet.ts.net:43804",
        "http://127.0.0.1:45404",
    ] {
        let said = Address::parse(written).err().context(written)?.to_string();
        assert_eq!(
            said,
            format!(
                "{written} is not an address a browser opens the chat at: write `https`, a host \
                 and an optional port, such as https://<machine>.<tailnet>.ts.net"
            )
        );
    }
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
            "https://100.64.0.7:8443",
            "100.64.0.7:8443",
            "https://100.64.0.7:8443",
        ),
        (
            "https://[fd7a:115c::7]",
            "[fd7a:115c::7]",
            "https://[fd7a:115c::7]",
        ),
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
    let stream = bearer(Method::GET, STREAM, &browser).body(Body::empty())?;
    assert_eq!(fixture.status(stream).await?, StatusCode::OK);
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
    let script = page("/index-Bq3nR7xK.js").await?;
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
        "/a/index-Bq3nR7xK.js",
        "//index-Bq3nR7xK.js",
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
    std::fs::write(custom.join("index-Bq3nR7xK.js"), "// custom script")?;
    let script = || async {
        fixture
            .send(request(Method::GET, "/index-Bq3nR7xK.js").body(Body::empty())?)
            .await
    };
    let custom_site = || async {
        let mut page = fixture.stream("conversations=1", &token).await?;
        anyhow::Ok(page.snapshot().await?["custom_site"].clone())
    };
    assert_eq!(script().await?.body, b"// release script");
    assert_eq!(custom_site().await?, false);
    std::fs::write(custom.join("index.html"), "<main>custom page</main>")?;
    assert_eq!(script().await?.body, b"// custom script");
    assert_eq!(custom_site().await?, true);
    // The copy is served whole: a file it lacks does not come from the release.
    std::fs::remove_file(custom.join("index-Bq3nR7xK.js"))?;
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
    mkfifo(
        root.join("release/pipe.js"),
        std::fs::Permissions::from_mode(0o600),
    )?;
    let custom = root.join("state/components/web/site");
    std::fs::create_dir_all(&custom)?;
    symlink(root.join("release/index.html"), custom.join("index.html"))?;
    for folder in [root.join("release"), custom.clone()] {
        symlink(root.join("secret.token"), folder.join("leak.js"))?;
        symlink(
            root.join("release/index-Bq3nR7xK.js"),
            folder.join("linked.js"),
        )?;
        symlink("index-Bq3nR7xK.js", folder.join("beside.js"))?;
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

#[tokio::test]
async fn a_browser_that_holds_its_connection_open_delays_a_stop_by_two_seconds_of_a_paused_clock()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));

    // A message whose upload never comes: the provider says it is ready
    // for the body, and the browser sends none of it.
    let mut browser = TcpStream::connect(address).await?;
    let upload = format!(
        "PUT /api/conversations/assistant/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/messages/\
         m-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b HTTP/1.1\r\nHost: {AUTHORITY}\r\n\
         Authorization: Bearer {token}\r\nContent-Type: multipart/form-data; boundary=b\r\n\
         Content-Length: 1000\r\nExpect: 100-continue\r\n\r\n"
    );
    browser.write_all(upload.as_bytes()).await?;
    let mut ready = [0; 25];
    browser.read_exact(&mut ready).await?;
    assert_eq!(&ready, b"HTTP/1.1 100 Continue\r\n\r\n");

    tokio::time::pause();
    let told = Instant::now();
    stop.notify_one();
    serving.await??;
    // A timer is a millisecond coarse.
    assert_eq!(told.elapsed().as_secs(), 2, "{:?}", told.elapsed());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_never_sends_its_request_is_closed_after_the_header_timeout() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));

    // One peer that opens a connection and sends nothing, and one that sends
    // part of a request line and no more.
    let silent = TcpStream::connect(address).await?;
    let mut slow = TcpStream::connect(address).await?;
    slow.write_all(b"GET /health HT").await?;
    tokio::task::yield_now().await;

    // Once the header timeout passes, the provider closes both without an
    // answer, so a read of each reaches the end rather than waiting forever.
    tokio::time::advance(HEADER_READ_TIMEOUT + Duration::from_secs(1)).await;
    for mut peer in [silent, slow] {
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), peer.read_to_end(&mut answer))
            .await
            .context("a connection was still open a header timeout after it stalled")??;
        assert_eq!(String::from_utf8_lossy(&answer), "");
    }

    stop.notify_one();
    serving.await??;
    Ok(())
}
