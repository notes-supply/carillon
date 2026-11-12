//! Files of core's attachment store, a message's and an answer's alike, by
//! signed link.
//!
//! A link is minted only in an answer to the signed-in page and carries its
//! own authority, so `<img>` and an ordinary download work without the
//! token. It names one file, expires, and dies with the browser key. Only
//! a raster image is shown in place. Anything else, HTML and SVG included,
//! downloads, so nothing an agent or a person made runs on this origin,
//! where it could read the token.

use std::{
    path::{Component, Path as FilePath},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{
        StatusCode,
        header::{
            CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY,
            CONTENT_TYPE,
        },
    },
    response::{IntoResponse as _, Response},
};
use carillon_installation::config::attachment_store;
use carillon_interface::InterfaceError;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::File,
    io::{AsyncReadExt as _, AsyncSeekExt as _},
};
use tokio_util::io::ReaderStream;

use crate::{browser::refusal, serve::Web};

const DAY: u64 = 24 * 60 * 60;
const BINARY: &str = "application/octet-stream";

/// A file as the page shows it. Never where it is on this host.
#[derive(Serialize)]
pub(crate) struct FileView {
    name: String,
    size: u64,
    /// The type the link answers with: a raster image's own, which the
    /// page may draw, or `application/octet-stream`, which downloads.
    #[serde(rename = "type")]
    media_type: &'static str,
    url: String,
}

/// What a link's query carries beside the path it signs.
#[derive(Deserialize)]
pub(crate) struct Link {
    name: String,
    exp: u64,
    sig: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// `text` with every byte a URL or a header parameter could misread
/// percent-encoded.
fn encoded(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn signed<'a>(path: &'a str, name: &'a str, expires: &'a str) -> [&'a str; 4] {
    ["file", path, name, expires]
}

/// The stored file `path` names, with its type when its bytes are a raster
/// image. A path that would leave the store names nothing.
async fn stored(web: &Web, path: &str) -> Option<(File, Option<&'static str>)> {
    let relative = FilePath::new(path);
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let store = attachment_store(&web.registry.value.data_dir);
    let mut file = File::open(store.join(relative)).await.ok()?;
    if !file.metadata().await.ok()?.is_file() {
        return None;
    }
    let mut start = [0; 12];
    let mut read = 0;
    while read < start.len() {
        match file.read(&mut start[read..]).await.ok()? {
            0 => break,
            more => read += more,
        }
    }
    file.rewind().await.ok()?;
    let raster = match &start[..read] {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P'] => Some("image/webp"),
        _ => None,
    };
    Some((file, raster))
}

/// The stored file `path` as the page shows it, with a link that reads it
/// until the end of the next day. The link is the same all day, so a
/// transcript read again does not make the browser fetch its images again.
pub(crate) async fn view(web: &Web, path: &str, name: &str, size: u64) -> FileView {
    let expires = ((now() / DAY + 2) * DAY).to_string();
    let signature = web.browser.sign(&signed(path, name, &expires));
    let segments: Vec<String> = path.split('/').map(encoded).collect();
    FileView {
        name: name.to_owned(),
        size,
        media_type: stored(web, path)
            .await
            .and_then(|(_, raster)| raster)
            .unwrap_or(BINARY),
        url: format!(
            "/files/{}?name={}&exp={expires}&sig={signature}",
            segments.join("/"),
            encoded(name)
        ),
    }
}

/// `GET /files/{path}?name=&exp=&sig=`: the bytes of a file the page was
/// given a link to.
pub(crate) async fn file(
    State(web): State<Arc<Web>>,
    Path(path): Path<String>,
    Query(link): Query<Link>,
) -> Result<Response, InterfaceError> {
    let expires = link.exp.to_string();
    if link.exp <= now()
        || !web
            .browser
            .matches(&signed(&path, &link.name, &expires), &link.sig)
    {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "This file link is no longer good. Read the conversation again for a new one.",
        ));
    }
    let gone = || refusal(StatusCode::NOT_FOUND, "Carillon does not have this file.");
    let (file, raster) = stored(&web, &path).await.ok_or_else(gone)?;
    let length = file.metadata().await.map_err(|_| gone())?.len();
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if raster.is_some() {
            "inline"
        } else {
            "attachment"
        },
        encoded(&link.name)
    );
    Ok((
        [
            (CONTENT_TYPE, raster.unwrap_or(BINARY).to_owned()),
            (CONTENT_DISPOSITION, disposition),
            (CONTENT_LENGTH, length.to_string()),
            (CONTENT_SECURITY_POLICY, "sandbox".to_owned()),
            // A stored file never changes, and its link changes daily.
            (
                CACHE_CONTROL,
                "private, max-age=86400, immutable".to_owned(),
            ),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response())
}

#[cfg(test)]
mod tests;
