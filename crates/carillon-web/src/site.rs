//! The page's files, read from disk on each request, so an edit shows on
//! the next reload with no build and no restart.
//!
//! A complete copy of the page in the provider's own `site` directory is
//! served instead of the release's. Either directory is flat, and the page
//! holds no data, so its files need no token.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    extract::State,
    http::{Method, StatusCode, Uri, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
};

use crate::serve::Web;

pub(crate) struct Site {
    /// The owner's copy, `site` in the provider's component directory.
    pub(crate) custom: PathBuf,
    /// The release's copy.
    pub(crate) release: PathBuf,
}

impl Site {
    /// Whether the owner's copy is the one served: it has a page of its own.
    pub(crate) fn is_custom(&self) -> bool {
        self.custom.join(INDEX).is_file()
    }

    fn root(&self) -> &Path {
        if self.is_custom() {
            &self.custom
        } else {
            &self.release
        }
    }
}

pub(crate) const INDEX: &str = "index.html";

/// The media type of a page file, by its extension.
fn media_type(name: &str) -> Option<&'static str> {
    let (stem, extension) = name.rsplit_once('.')?;
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if !plain(stem) || stem.starts_with('.') {
        return None;
    }
    Some(match extension {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => return None,
    })
}

/// `GET /` and `GET /<file>`: a file of the page, or 404.
pub(crate) async fn file(State(web): State<Arc<Web>>, method: Method, uri: Uri) -> Response {
    if method != Method::GET {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let name = match uri.path() {
        "/" => INDEX,
        path => path.strip_prefix('/').unwrap_or(path),
    };
    let Some(media_type) = media_type(name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(web.site.root().join(name)).await {
        Ok(bytes) => ([(CONTENT_TYPE, media_type)], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
