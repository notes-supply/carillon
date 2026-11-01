//! `carillon settings`: the settings pages interfaces and runtimes own, at
//! the terminal. It lists the routed channels, prints one channel's or one
//! agent's page, and applies changes left to right, each drawn from the
//! page read just before it. Owners decide; this draws what they answer.

use std::{fmt::Write as _, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use carillon_installation::{
    config::{LoadedConfig, Provider, ProviderRole},
    loopback,
};
use carillon_protocol::settings::{
    Change, Choice, Page, RuntimePart, RuntimeSettings, Setting, SettingsRequest, Subject, Value,
};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::{Deserialize, de::DeserializeOwned};

use super::operations::operator_url;

/// How long a read or a change may take. An interface relays a runtime's
/// answer within core's own deadline and re-reads once after a timeout.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);

/// A change named on the command line.
#[derive(Debug, PartialEq, Eq)]
enum Edit {
    Set { key: String, value: String },
    Reset { key: String },
}

impl Edit {
    fn key(&self) -> &str {
        let (Self::Set { key, .. } | Self::Reset { key }) = self;
        key
    }
}

/// Where a page is read and changed.
enum Owner {
    /// The page an interface provider serves for one of its channels.
    Channel(Url),
    /// An agent's own values, from its runtime through core.
    Agent { relay: Url, agent_id: String },
}

/// The operator's client of the owners' pages.
struct Operator {
    client: Client,
    token: String,
}

/// What `carillon settings` prints: with no channel or agent named, one
/// line per routed channel; else the named page once `words`, after the
/// channel, have changed it.
pub(super) async fn settings(
    config: &LoadedConfig,
    agent: Option<String>,
    words: &[String],
) -> Result<String> {
    let operator = Operator {
        // A change that may have reached its owner is never sent again.
        client: loopback::client()
            .retry(reqwest::retry::never())
            .timeout(ANSWER_TIMEOUT)
            .build()?,
        token: config.credential("operator")?,
    };
    let (owner, edits, mut page) = if let Some(agent_id) = agent {
        let edits = edits(words)?;
        let owner = Owner::Agent {
            relay: operator_url(config, "/v1/settings")?,
            agent_id,
        };
        let page = owner.page(&operator, None).await?;
        (owner, edits, page)
    } else if let Some((channel, words)) = words.split_first() {
        let edits = edits(words)?;
        let (owner, page) = served(config, &operator, channel).await?;
        (owner, edits, page)
    } else {
        return channels(config, &operator).await;
    };
    for (applied, edit) in edits.iter().enumerate() {
        let next = match change(&page, edit) {
            // An owner's refusal need not say which line it refused.
            Ok(change) => owner
                .page(&operator, Some(change))
                .await
                .with_context(|| edit.key().to_owned()),
            Err(failure) => Err(failure),
        };
        page = next.map_err(|failure| stopped(failure, &edits[..applied]))?;
    }
    Ok(render(&page))
}

fn edits(words: &[String]) -> Result<Vec<Edit>> {
    let mut words = words.iter();
    let mut edits = Vec::new();
    while let Some(word) = words.next() {
        edits.push(if let Some((key, value)) = word.split_once('=') {
            Edit::Set {
                key: key.to_owned(),
                value: value.to_owned(),
            }
        } else if word == "reset" {
            Edit::Reset {
                key: words
                    .next()
                    .context("reset names the setting it resets: reset <key>")?
                    .clone(),
            }
        } else {
            bail!("{word} is neither key=value nor reset <key>");
        });
    }
    Ok(edits)
}

/// `failure`, saying which changes were applied before the one that failed.
fn stopped(failure: anyhow::Error, applied: &[Edit]) -> anyhow::Error {
    if applied.is_empty() {
        return failure;
    }
    let keys: Vec<&str> = applied.iter().map(Edit::key).collect();
    anyhow!("{failure:#}\nAlready applied: {}.", keys.join(", "))
}

/// The change `edit` asks of `page`, refused before it is sent when its key
/// names no line of the page, or none the runtime could show, or its value
/// no choice of the line.
fn change(page: &Page, edit: &Edit) -> Result<Change> {
    let key = edit.key();
    let setting = line(page, key).ok_or_else(|| match &page.runtime {
        RuntimePart::Unavailable { reason } => anyhow!("{key} cannot be changed now. {reason}"),
        RuntimePart::Settings(_) => anyhow!(
            "{key} is not a setting of {}. Its settings are:\n{}",
            page.title,
            listed(page.settings().map(|setting| (&setting.id, &setting.name)))
        ),
    })?;
    let value = match edit {
        Edit::Set { value, .. } => Some(Value::One {
            id: choice(setting, value)?.id.clone(),
        }),
        Edit::Reset { .. } => None,
    };
    Ok(Change {
        setting: setting.id.clone(),
        value,
        expected: setting.revision.clone(),
    })
}

/// The line whose ID is `key`, or else the one line whose ID ends in
/// `.<key>`, so `thinking` names `model.thinking`.
fn line<'a>(page: &'a Page, key: &str) -> Option<&'a Setting> {
    let suffix = format!(".{key}");
    page.settings()
        .find(|setting| setting.id == key)
        .or_else(|| {
            only(
                page.settings()
                    .filter(|setting| setting.id.ends_with(&suffix)),
            )
        })
}

/// The choice whose ID is `value`, or else the one whose name it is, in any
/// case, or else the one whose ID ends in `/<value>`, so `gpt-6.1-sol`
/// names `openai-codex/gpt-6.1-sol`.
fn choice<'a>(setting: &'a Setting, value: &str) -> Result<&'a Choice> {
    let value_name = value.to_lowercase();
    let suffix = format!("/{value}");
    setting
        .choices
        .iter()
        .find(|choice| choice.id == value)
        .or_else(|| {
            only(
                setting
                    .choices
                    .iter()
                    .filter(|choice| choice.name.to_lowercase() == value_name),
            )
        })
        .or_else(|| {
            only(
                setting
                    .choices
                    .iter()
                    .filter(|choice| choice.id.ends_with(&suffix)),
            )
        })
        .ok_or_else(|| {
            let refused = if value.is_empty() {
                format!("{} needs a value", setting.name)
            } else {
                format!("{value} is not a choice of {}", setting.name)
            };
            anyhow!(
                "{refused}. Its choices are:\n{}",
                listed(
                    setting
                        .choices
                        .iter()
                        .map(|choice| (&choice.id, &choice.name))
                )
            )
        })
}

fn only<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let first = items.next()?;
    items.next().is_none().then_some(first)
}

/// `id  name` lines, the names aligned.
fn listed<'a>(entries: impl Iterator<Item = (&'a String, &'a String)>) -> String {
    let entries: Vec<_> = entries.collect();
    let width = entries
        .iter()
        .map(|(id, _)| id.chars().count())
        .max()
        .unwrap_or_default()
        + 2;
    entries
        .iter()
        .map(|(id, name)| format!("{id:<width$}{name}"))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Owner {
    /// The page, after `change` when there is one.
    async fn page(&self, operator: &Operator, change: Option<Change>) -> Result<Page> {
        match self {
            Self::Channel(url) => {
                let request = match change {
                    None => operator.client.get(url.clone()),
                    Some(change) => operator.client.put(url.clone()).json(&change),
                };
                answer(operator.send(request).await?).await
            }
            Self::Agent { relay, agent_id } => {
                let subject = Subject::Agent {
                    agent_id: agent_id.clone(),
                };
                let request = SettingsRequest {
                    subject: subject.clone(),
                    change,
                };
                let settings: RuntimeSettings = answer(
                    operator
                        .send(operator.client.post(relay.clone()).json(&request))
                        .await?,
                )
                .await?;
                Ok(Page {
                    title: agent_id.clone(),
                    subject,
                    interface: Vec::new(),
                    runtime: RuntimePart::Settings(settings),
                })
            }
        }
    }
}

impl Operator {
    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        let request = request.bearer_auth(&self.token).build()?;
        let url = request.url().clone();
        self.client
            .execute(request)
            .await
            .with_context(|| format!("{url} did not answer"))
    }
}

/// What an owner answered, or its refusal as it phrased it.
async fn answer<T: DeserializeOwned>(response: Response) -> Result<T> {
    #[derive(Deserialize)]
    struct Refusal {
        error: String,
    }
    let status = response.status();
    if status.is_success() {
        return response
            .json()
            .await
            .context("the answer could not be read");
    }
    match response.json::<Refusal>().await {
        Ok(refusal) => bail!("{}", refusal.error),
        Err(_) => bail!("refused with {status}"),
    }
}

/// The installation's enabled interface providers, which serve their
/// channels' pages.
fn interfaces(config: &LoadedConfig) -> impl Iterator<Item = &Provider> {
    config
        .value
        .providers
        .iter()
        .filter(|provider| provider.role == ProviderRole::Interface && provider.enabled)
}

/// `path` under a provider's URL, each segment escaped.
fn endpoint(provider: &Provider, path: &[&str]) -> Result<Url> {
    let mut url =
        Url::parse(&provider.url).with_context(|| format!("{} has an invalid URL", provider.id))?;
    url.path_segments_mut()
        .map_err(|()| anyhow!("{} has an invalid URL", provider.id))?
        .pop_if_empty()
        .extend(path);
    Ok(url)
}

/// The page of `channel` from the interface provider that routes it.
async fn served(
    config: &LoadedConfig,
    operator: &Operator,
    channel: &str,
) -> Result<(Owner, Page)> {
    for provider in interfaces(config) {
        let url = endpoint(provider, &["v1", "channels", channel, "settings"])?;
        let response = operator.send(operator.client.get(url.clone())).await?;
        if response.status() == StatusCode::NOT_FOUND {
            continue;
        }
        let page = answer(response).await?;
        return Ok((Owner::Channel(url), page));
    }
    bail!("no interface routes channel {channel}; `carillon settings` lists the routed channels")
}

/// Every routed channel of every enabled interface provider, one line each.
async fn channels(config: &LoadedConfig, operator: &Operator) -> Result<String> {
    let mut pages = Vec::new();
    for provider in interfaces(config) {
        let url = endpoint(provider, &["v1", "channels"])?;
        let listed: Vec<Page> = answer(operator.send(operator.client.get(url)).await?)
            .await
            .with_context(|| format!("{} did not list its channels", provider.id))?;
        pages.extend(listed);
    }
    Ok(summary(&pages))
}

/// One line per page: its subject's ID, its title and the values that
/// apply there, then why its runtime's lines are missing, if they are.
fn summary(pages: &[Page]) -> String {
    let width = |text: fn(&Page) -> &str| {
        pages
            .iter()
            .map(|page| text(page).chars().count())
            .max()
            .unwrap_or_default()
            + 2
    };
    let (id_width, title_width) = (width(subject_id), width(|page| page.title.as_str()));
    let mut text = String::new();
    for page in pages {
        let values: Vec<String> = page.settings().filter_map(Setting::value_name).collect();
        let mut line = format!(
            "{:<id_width$}{:<title_width$}{}",
            subject_id(page),
            page.title,
            values.join(", ")
        );
        if let RuntimePart::Unavailable { reason } = &page.runtime {
            let _ = write!(line, "  ⚠ {reason}");
        }
        let _ = writeln!(text, "{}", line.trim_end());
    }
    text
}

/// The ID an operator names a page's subject by.
fn subject_id(page: &Page) -> &str {
    match &page.subject {
        Subject::Channel { channel_id, .. } => channel_id,
        Subject::Agent { agent_id } => agent_id,
    }
}

/// The page as the terminal shows it: why the runtime's lines are missing,
/// if they are, the title, then its lines.
fn render(page: &Page) -> String {
    let mut text = String::new();
    if let RuntimePart::Unavailable { reason } = &page.runtime {
        let _ = writeln!(text, "⚠ {reason}");
    }
    let _ = writeln!(text, "{}", page.title);
    for line in page.lines() {
        let _ = writeln!(text, "{line}");
    }
    text
}

#[cfg(test)]
mod tests;
