//! The page's files, read from disk on each request, so an edit shows on
//! the next reload with no build and no restart.
//!
//! A complete copy of the page in the provider's own `site` directory is
//! served instead of the release's. Either directory is flat, and the page
//! holds no data, so its files need no token. For that reason only a file
//! that is itself in the directory is served: a link there could name
//! anything the installing user reads. The directory, and the path to it,
//! may be reached through links: the owner names where the page lives, and
//! whatever directory that leads to is the page.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    extract::State,
    http::{Method, StatusCode, Uri, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
};
use tokio::{fs::OpenOptions, io::AsyncReadExt as _};

use crate::serve::Web;

pub(crate) struct Site {
    /// The owner's copy, `site` in the provider's component directory.
    pub(crate) custom: PathBuf,
    /// The release's copy.
    pub(crate) release: PathBuf,
}

impl Site {
    /// The owner's copy when it has a page of its own, or else the release's.
    fn root(&self) -> &Path {
        if has_page(&self.custom) {
            &self.custom
        } else {
            &self.release
        }
    }
}

const INDEX: &str = "index.html";

/// Whether `directory` has the page's `index.html`, as a file of its own.
pub(crate) fn has_page(directory: &Path) -> bool {
    std::fs::symlink_metadata(directory.join(INDEX)).is_ok_and(|found| found.is_file())
}

/// The media type of a page file, by its extension. The directory is
/// flat and has no hidden file.
fn media_type(name: &str) -> Option<&'static str> {
    if name.starts_with('.') || name.contains('/') {
        return None;
    }
    Some(match name.rsplit_once('.')?.1 {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => return None,
    })
}

/// The bytes of the regular file `name` in `directory`. The open itself
/// refuses a link, and what it opened is then asked what it is, so nothing
/// can be swapped in between.
async fn read(directory: &Path, name: &str) -> Option<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        // Without blocking, so a named pipe is refused and not waited for.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.join(name))
        .await
        .ok()?;
    if !file.metadata().await.ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.ok()?;
    Some(bytes)
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
    match read(web.site.root(), name).await {
        Some(bytes) => ([(CONTENT_TYPE, media_type)], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
