//! Signed file links: what a link opens, how it is answered and when it
//! stops working.

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{Method, StatusCode},
};

use super::{encoded, signed, view};
use crate::fixture::{Answer, Fixture, operator, request};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\npicture bytes";
const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>";
const HTML: &[u8] = b"<!doctype html><script>alert(localStorage.token)</script>";

async fn fixture() -> Result<Fixture> {
    let fixture = Fixture::new().await?;
    let capture = fixture.directory.path().join("state/attachments/capture");
    std::fs::create_dir_all(&capture)?;
    for (ordinal, bytes) in [PNG, SVG, HTML, b"\xff\xd8\xff\xe0 jpeg", b"GIF89a gif"]
        .iter()
        .enumerate()
    {
        std::fs::write(capture.join(ordinal.to_string()), bytes)?;
    }
    std::fs::write(capture.join("5"), b"RIFF\x10\0\0\0WEBPVP8 ")?;
    std::fs::write(fixture.directory.path().join("state/secret"), b"core's own")?;
    Ok(fixture)
}

/// The link the page is given to the stored file `path`.
async fn link(fixture: &Fixture, path: &str, name: &str) -> Result<String> {
    let file = serde_json::to_value(view(&fixture.web, path, name, 1).await)?;
    Ok(file["url"].as_str().context("no link")?.to_owned())
}

/// A link carries its own authority: it is followed with no token.
async fn follow(fixture: &Fixture, link: &str) -> Result<Answer> {
    fixture
        .send(request(Method::GET, link).body(Body::empty())?)
        .await
}

#[tokio::test]
async fn only_a_raster_image_is_shown_in_place_and_all_else_downloads() -> Result<()> {
    let fixture = fixture().await?;
    for (ordinal, name, media_type, bytes) in [
        (0, "chart.png", "image/png", PNG),
        (3, "photo.jpg", "image/jpeg", &b"\xff\xd8\xff\xe0 jpeg"[..]),
        (4, "loop.gif", "image/gif", b"GIF89a gif"),
        (5, "small.webp", "image/webp", b"RIFF\x10\0\0\0WEBPVP8 "),
    ] {
        let answer = follow(
            &fixture,
            &link(&fixture, &format!("capture/{ordinal}"), name).await?,
        )
        .await?;
        assert_eq!(answer.status, StatusCode::OK, "{name}");
        assert_eq!(answer.body, bytes);
        assert_eq!(answer.header("content-type"), media_type);
        assert_eq!(answer.header("content-length"), bytes.len().to_string());
        assert!(answer.header("content-disposition").starts_with("inline;"));
        assert_eq!(answer.header("content-security-policy"), "sandbox");
    }
    // What a browser would run is never given a type it runs, whatever its
    // name claims.
    for (ordinal, name, bytes) in [
        (1, "drawing.svg", SVG),
        (2, "page.html", HTML),
        (1, "really.png", SVG),
    ] {
        let answer = follow(
            &fixture,
            &link(&fixture, &format!("capture/{ordinal}"), name).await?,
        )
        .await?;
        assert_eq!(answer.status, StatusCode::OK, "{name}");
        assert_eq!(answer.body, bytes);
        assert_eq!(answer.header("content-type"), "application/octet-stream");
        assert!(
            answer
                .header("content-disposition")
                .starts_with("attachment;")
        );
        assert_eq!(answer.header("content-security-policy"), "sandbox");
        assert_eq!(answer.header("x-content-type-options"), "nosniff");
    }
    Ok(())
}

#[tokio::test]
async fn a_link_opens_one_file_until_it_expires_or_the_key_changes() -> Result<()> {
    let fixture = fixture().await?;
    let good = link(&fixture, "capture/0", "chart.png").await?;
    assert_eq!(follow(&fixture, &good).await?.status, StatusCode::OK);
    // The same link all day, so the browser keeps what it fetched.
    assert_eq!(link(&fixture, "capture/0", "chart.png").await?, good);

    let (path, query) = good.split_once('?').context("no query")?;
    let (unsigned, signature) = query.rsplit_once("sig=").context("no signature")?;
    let flipped: String = signature
        .chars()
        .enumerate()
        .map(|(index, digit)| match (index, digit) {
            (0, '0') => '1',
            (0, _) => '0',
            (_, digit) => digit,
        })
        .collect();
    let expiry = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("exp="))
        .context("no expiry")?;
    let later = (expiry.parse::<u64>()? + 1).to_string();
    for tampered in [
        good.replace("capture/0", "capture/2"),
        good.replace("chart.png", "page.html"),
        good.replace(&format!("exp={expiry}"), &format!("exp={later}")),
        format!("{path}?{unsigned}sig="),
        format!("{path}?{unsigned}sig={flipped}"),
        format!("{good}00"),
        good.replace(signature, "not-hex"),
    ] {
        let answer = follow(&fixture, &tampered).await?;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{tampered}");
        assert!(answer.body.len() < 200 && !answer.body.starts_with(PNG));
    }
    let bare = follow(&fixture, path).await?;
    assert_eq!(bare.status, StatusCode::BAD_REQUEST);

    // Signed by the current key, but for a time already past.
    for past in ["1", "1700000000"] {
        let signature = fixture
            .web
            .browser
            .sign(&signed("capture/0", "chart.png", past));
        let expired = format!("/files/capture/0?name=chart.png&exp={past}&sig={signature}");
        assert_eq!(
            follow(&fixture, &expired).await?.status,
            StatusCode::FORBIDDEN
        );
    }

    let signed_out = fixture
        .send(operator(Method::POST, "/v1/sign-out").body(Body::empty())?)
        .await?;
    assert_eq!(signed_out.status, StatusCode::NO_CONTENT);
    assert_eq!(follow(&fixture, &good).await?.status, StatusCode::FORBIDDEN);
    let renewed = link(&fixture, "capture/0", "chart.png").await?;
    assert_ne!(renewed, good);
    assert_eq!(follow(&fixture, &renewed).await?.status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_signed_path_that_leaves_the_store_names_nothing() -> Result<()> {
    let fixture = fixture().await?;
    let future = "99999999999";
    for path in [
        "../secret",
        "capture/../../secret",
        "/etc/hostname",
        "capture",
        "capture/9",
        "",
    ] {
        let signature = fixture.web.browser.sign(&signed(path, "x", future));
        let link = format!(
            "/files/{}?name=x&exp={future}&sig={signature}",
            path.split('/').map(encoded).collect::<Vec<_>>().join("/")
        );
        let answer = follow(&fixture, &link).await?;
        assert!(
            answer.status == StatusCode::NOT_FOUND || answer.status == StatusCode::FORBIDDEN,
            "{path} answered {}",
            answer.status
        );
        assert!(
            !answer.text().contains("core's own"),
            "{path} left the store"
        );
    }
    // A link's view of a path outside the store has no type of its own.
    let outside = serde_json::to_value(view(&fixture.web, "../secret", "x", 1).await)?;
    assert_eq!(outside["type"], "application/octet-stream");
    Ok(())
}

#[tokio::test]
async fn any_file_name_makes_a_header_a_browser_can_read() -> Result<()> {
    let fixture = fixture().await?;
    for (name, plain, extended) in [
        (
            "quarterly \"final\" report.pdf",
            "quarterly _final_ report.pdf",
            "quarterly%20%22final%22%20report.pdf",
        ),
        (
            "line\r\nSet-Cookie: a=b.txt",
            "line__Set-Cookie_ a_b.txt",
            "line%0D%0ASet-Cookie%3A%20a%3Db.txt",
        ),
        (
            "résumé 履歴書.pdf",
            "r_sum_ ___.pdf",
            "r%C3%A9sum%C3%A9%20%E5%B1%A5%E6%AD%B4%E6%9B%B8.pdf",
        ),
        (
            "../../etc/passwd",
            ".._.._etc_passwd",
            "..%2F..%2Fetc%2Fpasswd",
        ),
        (
            "semi;colon\\back.txt",
            "semi_colon_back.txt",
            "semi%3Bcolon%5Cback.txt",
        ),
    ] {
        let answer = follow(&fixture, &link(&fixture, "capture/2", name).await?).await?;
        assert_eq!(answer.status, StatusCode::OK, "{name}");
        assert_eq!(
            answer.header("content-disposition"),
            format!("attachment; filename=\"{plain}\"; filename*=UTF-8''{extended}")
        );
        assert!(!answer.parts.headers.contains_key("set-cookie"));
    }
    Ok(())
}
