//! Downloading an admitted message's files into the provider's staging for
//! core to capture.
//!
//! Only an authenticated, admitted and activated message reaches here. Files
//! come from Discord's attachment CDN over HTTPS without redirects or bot
//! credentials, are streamed to disk within their declared length, and are
//! staged under generated names: uploaders' file names are display text.
//! Staging is this provider's scratch space, not accepted storage: it is
//! removed once core answers, and at startup.

use std::{
    io,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use carillon_installation::config::MIB;
use carillon_protocol::model::{StagedAttachments, StagedFile};
use reqwest::{StatusCode, Url, redirect};
use tokio::{
    fs,
    io::AsyncWriteExt as _,
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, timeout, timeout_at},
};
use tracing::warn;
use twilight_http::{
    Client as DiscordHttp, error::ErrorType as DiscordErrorType,
    response::StatusCode as DiscordStatusCode,
};
use twilight_model::id::Id;

use crate::{
    config::AttachmentPolicy,
    conversations::{MessagePlan, SourceAttachment},
};

/// A download that receives nothing for this long has failed.
const IDLE: Duration = Duration::from_mins(1);
/// Attempts per file for failures worth repeating, such as a dropped connection.
const ATTEMPTS: u32 = 3;
const KIB: u64 = 1024;

/// Why a message's files could not be staged, in words for its sender.
#[derive(Debug)]
pub(crate) struct Refusal(pub(crate) String);

pub(crate) struct Downloads {
    client: reqwest::Client,
    cdn: Cdn,
    staging: PathBuf,
    policy: AttachmentPolicy,
    active: Arc<Semaphore>,
    room: Arc<Semaphore>,
}

/// Where attachment bytes may come from.
#[derive(Clone, Debug)]
enum Cdn {
    Discord,
    #[cfg(test)]
    Fixture(u16),
}

impl Cdn {
    /// The address to fetch, when it is a message attachment on the CDN.
    fn url(&self, attachment: &SourceAttachment) -> Option<Url> {
        let (scheme, hosts, port): (&str, &[&str], Option<u16>) = match self {
            Self::Discord => (
                "https",
                &["cdn.discordapp.com", "media.discordapp.net"],
                None,
            ),
            #[cfg(test)]
            Self::Fixture(port) => ("http", &["127.0.0.1"], Some(*port)),
        };
        let url = Url::parse(&attachment.url).ok()?;
        let segments = url.path_segments()?.collect::<Vec<_>>();
        (url.scheme() == scheme
            && url.host_str().is_some_and(|host| hosts.contains(&host))
            && url.port() == port
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && matches!(
                segments.as_slice(),
                ["attachments", channel, id, name]
                    if is_snowflake(channel) && *id == attachment.id && !name.is_empty()
            ))
        .then_some(url)
    }
}

fn is_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|id| id != 0 && id.to_string() == value)
}

/// An address without its signed query, for logs.
fn redacted(url: &Url) -> String {
    format!("{}{}", url.host_str().unwrap_or_default(), url.path())
}

/// A file name fit for an explanation: one line, no markup, bounded.
pub(crate) fn display_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .filter(|character| !character.is_control() && !"`*_~|<>@#[]()\\".contains(*character))
        .take(80)
        .collect::<String>();
    if cleaned.trim().is_empty() {
        "a file".to_owned()
    } else {
        format!("`{}`", cleaned.trim())
    }
}

/// The files of one message, downloaded under generated names, with the
/// staging room they hold. Dropping it removes them and then frees the room.
pub(crate) struct Staged {
    directory: PathBuf,
    pub(crate) manifest: StagedAttachments,
    room: Option<OwnedSemaphorePermit>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let (directory, room) = (std::mem::take(&mut self.directory), self.room.take());
        let remove = move || {
            if let Err(error) = std::fs::remove_dir_all(&directory)
                && error.kind() != io::ErrorKind::NotFound
            {
                warn!(%error, "staged attachments could not be removed; the provider removes them at startup");
            }
            drop(room);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

enum Failure {
    /// The CDN no longer serves this address; the message may have a fresh one.
    Expired,
    /// Worth another attempt.
    Transient(String),
    Refused(Refusal),
}

impl Downloads {
    /// Prepare `staging`, removing what a previous instance left there.
    /// Call only once this instance owns the provider's state.
    pub(crate) fn open(staging: PathBuf, policy: AttachmentPolicy) -> Result<Self> {
        match std::fs::remove_dir_all(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("clear {}", staging.display()));
            }
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&staging)
            .with_context(|| format!("create {}", staging.display()))?;
        Ok(Self {
            // Never redirect, and never send the bot token: CDN downloads
            // use a client of their own.
            client: reqwest::Client::builder()
                .redirect(redirect::Policy::none())
                .https_only(true)
                .connect_timeout(IDLE)
                .build()?,
            cdn: Cdn::Discord,
            staging,
            active: Arc::new(Semaphore::new(policy.concurrent_messages as usize)),
            room: Arc::new(Semaphore::new(kib(policy.staging_mib * MIB))),
            policy,
        })
    }

    /// Stand-in CDN for tests: plain HTTP on a loopback port, otherwise the
    /// same address rules.
    #[cfg(test)]
    pub(crate) fn with_fixture_cdn(mut self, port: u16) -> Self {
        self.cdn = Cdn::Fixture(port);
        self.client = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .build()
            .unwrap_or_default();
        self
    }

    /// Take one of the slots for active attachment work: downloading, or
    /// waiting for core to capture. Waiting out a retry holds none.
    pub(crate) async fn active(&self) -> Option<OwnedSemaphorePermit> {
        self.active.clone().acquire_owned().await.ok()
    }

    /// Check what Discord declared against the policy before fetching.
    pub(crate) fn admit(&self, files: &[SourceAttachment]) -> Result<(), Refusal> {
        let policy = &self.policy;
        if !policy.enabled {
            return Err(Refusal(
                "Attachments are not enabled here, so this message was not accepted.".to_owned(),
            ));
        }
        if files.len() > policy.max_files as usize {
            return Err(Refusal(format!(
                "Carillon accepts at most {} attachments per message, so this message was not accepted.",
                policy.max_files
            )));
        }
        let total: u64 = files.iter().map(|file| file.size).sum();
        if files
            .iter()
            .any(|file| file.size > policy.max_file_mib * MIB)
            || total > policy.max_message_mib * MIB
        {
            return Err(Refusal(format!(
                "Carillon accepts attachments of up to {} MiB each and {} MiB per message, so this message was not accepted.",
                policy.max_file_mib, policy.max_message_mib
            )));
        }
        if let Some(file) = files.iter().find(|file| self.cdn.url(file).is_none()) {
            return Err(Refusal(format!(
                "{} is not a Discord attachment Carillon can download, so this message was not accepted.",
                display_name(&file.filename)
            )));
        }
        Ok(())
    }

    /// Download the message's files in order, waiting for staging room
    /// first. With `http`, a file whose address expired is looked up once
    /// more on the same message; a replaced or deleted file fails the whole
    /// message.
    pub(crate) async fn stage(
        &self,
        http: Option<&DiscordHttp>,
        plan: &MessagePlan,
        deadline: Instant,
    ) -> Result<Staged, Refusal> {
        let late = || {
            Refusal(
                "Carillon could not finish downloading these attachments in time, so this message was not accepted. Please send it again."
                    .to_owned(),
            )
        };
        let total = plan.attachments.iter().map(|file| file.size).sum();
        let room = timeout_at(
            deadline,
            self.room.clone().acquire_many_owned(kib_u32(total)),
        )
        .await
        .map_err(|_| late())?
        .map_err(|_| late())?;
        let directory = self.staging.join(uuid::Uuid::now_v7().simple().to_string());
        let mut staged = Staged {
            manifest: StagedAttachments {
                batch: directory
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                files: Vec::with_capacity(plan.attachments.len()),
            },
            directory,
            room: Some(room),
        };
        let unavailable = |error: io::Error| {
            warn!(%error, "attachment staging failed");
            Refusal(
                "Carillon could not store these attachments for checking, so this message was not accepted."
                    .to_owned(),
            )
        };
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&staged.directory)
            .map_err(unavailable)?;
        let _active = timeout_at(deadline, self.active())
            .await
            .map_err(|_| late())?;
        for (ordinal, file) in plan.attachments.iter().enumerate() {
            let destination = staged.directory.join(ordinal.to_string());
            timeout_at(deadline, self.fetch_file(http, plan, file, &destination))
                .await
                .map_err(|_| late())??;
            staged.manifest.files.push(StagedFile {
                name: file.filename.clone(),
                size: file.size,
                content_type: file.content_type.clone(),
            });
        }
        Ok(staged)
    }

    async fn fetch_file(
        &self,
        http: Option<&DiscordHttp>,
        plan: &MessagePlan,
        file: &SourceAttachment,
        destination: &Path,
    ) -> Result<(), Refusal> {
        let name = display_name(&file.filename);
        let mut current = file.clone();
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            let Some(url) = self.cdn.url(&current) else {
                return Err(Refusal(format!(
                    "{name} is not a Discord attachment Carillon can download, so this message was not accepted."
                )));
            };
            attempt += 1;
            match self.download(&url, file.size, destination).await {
                Ok(()) => return Ok(()),
                Err(Failure::Refused(refusal)) => return Err(refusal),
                Err(Failure::Expired) => match http {
                    Some(http) if !refreshed => {
                        refreshed = true;
                        current = refresh(http, plan, file).await?;
                    }
                    _ => {
                        return Err(Refusal(format!(
                            "Discord no longer serves {name}, so this message was not accepted."
                        )));
                    }
                },
                Err(Failure::Transient(reason)) if attempt < ATTEMPTS => {
                    warn!(file = %redacted(&url), %reason, "attachment download failed; trying again");
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                }
                Err(Failure::Transient(reason)) => {
                    warn!(file = %redacted(&url), %reason, "attachment download failed");
                    return Err(Refusal(format!(
                        "Carillon could not download {name}, so this message was not accepted. Please send it again."
                    )));
                }
            }
        }
    }

    /// Stream one file to `destination`, never beyond its declared length.
    async fn download(&self, url: &Url, size: u64, destination: &Path) -> Result<(), Failure> {
        let transient = |error: reqwest::Error| Failure::Transient(error.without_url().to_string());
        let mut response = timeout(IDLE, self.client.get(url.clone()).send())
            .await
            .map_err(|_| Failure::Transient("no response".to_owned()))?
            .map_err(transient)?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => {
                return Err(Failure::Expired);
            }
            status if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS => {
                return Err(Failure::Transient(format!("CDN answered {status}")));
            }
            status => {
                warn!(file = %redacted(url), %status, "attachment download refused");
                return Err(Failure::Refused(Refusal(
                    "Discord refused to provide an attachment, so this message was not accepted."
                        .to_owned(),
                )));
            }
        }
        let mismatch = || {
            Failure::Refused(Refusal(
                "An attachment did not match the size Discord announced, so this message was not accepted."
                    .to_owned(),
            ))
        };
        if response
            .content_length()
            .is_some_and(|length| length != size)
        {
            return Err(mismatch());
        }
        let part = destination.with_extension("part");
        let local = |error: io::Error| {
            warn!(%error, "attachment staging write failed");
            Failure::Refused(Refusal(
                "Carillon could not store an attachment for checking, so this message was not accepted."
                    .to_owned(),
            ))
        };
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&part)
            .await
            .map_err(local)?;
        let mut written = 0_u64;
        while let Some(chunk) = timeout(IDLE, response.chunk())
            .await
            .map_err(|_| Failure::Transient("the download stalled".to_owned()))?
            .map_err(transient)?
        {
            written += chunk.len() as u64;
            if written > size {
                return Err(mismatch());
            }
            output.write_all(&chunk).await.map_err(local)?;
        }
        if written != size {
            return Err(Failure::Transient(
                "the download ended before the announced size".to_owned(),
            ));
        }
        output.flush().await.map_err(local)?;
        drop(output);
        fs::rename(&part, destination).await.map_err(local)
    }
}

/// The same attachment of the same message, as Discord describes it now.
async fn refresh(
    http: &DiscordHttp,
    plan: &MessagePlan,
    file: &SourceAttachment,
) -> Result<SourceAttachment, Refusal> {
    let name = display_name(&file.filename);
    let gone = || {
        Refusal(format!(
            "{name} was removed or replaced before Carillon could download it, so this message was not accepted."
        ))
    };
    let unavailable = || {
        Refusal(format!(
            "Carillon could not download {name}, so this message was not accepted. Please send it again."
        ))
    };
    let (Ok(channel), Ok(message)) = (
        plan.channel_id.parse::<u64>(),
        plan.message_id.parse::<u64>(),
    ) else {
        return Err(unavailable());
    };
    let (Some(channel), Some(message)) = (Id::new_checked(channel), Id::new_checked(message))
    else {
        return Err(unavailable());
    };
    let message = match http.message(channel, message).await {
        Ok(response) => response.model().await.map_err(|_| unavailable())?,
        Err(error)
            if matches!(error.kind(), DiscordErrorType::Response { status, .. }
                if *status == DiscordStatusCode::NOT_FOUND || *status == DiscordStatusCode::FORBIDDEN) =>
        {
            return Err(gone());
        }
        Err(error) => {
            warn!(%error, "could not look up an expired attachment");
            return Err(unavailable());
        }
    };
    let current = message
        .attachments
        .into_iter()
        .find(|candidate| candidate.id.to_string() == file.id)
        .ok_or_else(gone)?;
    if current.size != file.size {
        return Err(gone());
    }
    Ok(SourceAttachment {
        url: current.url,
        ..file.clone()
    })
}

/// Bytes as the KiB the staging room is counted in, rounded up.
fn kib(bytes: u64) -> usize {
    usize::try_from(bytes.div_ceil(KIB)).unwrap_or(usize::MAX)
}

fn kib_u32(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(KIB)).unwrap_or(u32::MAX)
}

#[cfg(test)]
pub(crate) mod tests;
