//! `carillon settings`: the settings pages interfaces and runtimes own, at
//! the terminal. It lists each interface's routed channels, prints one
//! channel's or one agent's page, and applies changes left to right, each
//! drawn from the page read just before it. A channel belongs to the
//! interface the operator names, and no other interface is asked about it.
//! Owners decide; this draws what they answer.

use std::{fmt::Write as _, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use carillon_installation::{
    config::{LoadedConfig, Provider, ProviderRole},
    loopback,
};
use carillon_protocol::settings::{
    Change, Choice, Page, RuntimePart, RuntimeSettings, Setting, SettingsRequest, Subject, Value,
};
use reqwest::{Client, RequestBuilder, Response, Url};
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

pub(super) enum Named {
    Agent(String),
    Interface(Option<String>),
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
/// line per routed channel under its interface; else the named page once
/// `words`, after the channel, have changed it.
pub(super) async fn settings(
    config: &LoadedConfig,
    named: Named,
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
    let (owner, words) = match (named, words.split_first()) {
        (Named::Agent(agent_id), _) => {
            let relay = operator_url(config, "/v1/settings")?;
            (Owner::Agent { relay, agent_id }, words)
        }
        (Named::Interface(interface), Some((channel, words))) => {
            let provider = addressed(config, interface.as_deref())?;
            let page = endpoint(provider, &["v1", "channels", channel, "settings"])?;
            (Owner::Channel(page), words)
        }
        (Named::Interface(interface), None) => {
            return channels(config, interface.as_deref(), &operator).await;
        }
    };
    let edits = edits(words)?;
    let mut page = owner.page(&operator, None).await?;
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
/// no choice of a line that offers some. A line of many values takes a
/// comma list.
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
        Edit::Set { value, .. } => Some(match setting.value {
            Some(Value::Many { .. }) => Value::Many {
                ids: value
                    .split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| chosen(setting, item))
                    .collect::<Result<_>>()?,
            },
            _ => Value::One {
                id: chosen(setting, value)?,
            },
        }),
        Edit::Reset { .. } => None,
    };
    Ok(Change {
        setting: setting.id.clone(),
        value,
        expected: setting.revision.clone(),
    })
}

/// The ID of the choice `value` names. A line that offers no choice takes
/// the value as written, so its owner says why it cannot change.
fn chosen(setting: &Setting, value: &str) -> Result<String> {
    if setting.choices.is_empty() {
        return Ok(value.to_owned());
    }
    Ok(choice(setting, value)?.id.clone())
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
                    subject: Some(subject),
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

fn addressed<'a>(config: &'a LoadedConfig, named: Option<&str>) -> Result<&'a Provider> {
    let enabled: Vec<&Provider> = interfaces(config).collect();
    let ids = || {
        let ids: Vec<&str> = enabled
            .iter()
            .map(|provider| provider.id.as_str())
            .collect();
        ids.join(", ")
    };
    match (named, enabled.as_slice()) {
        (Some(id), _) => (enabled.iter().copied())
            .find(|provider| provider.id == id)
            .with_context(|| format!("{id} is not an enabled interface of: {}", ids())),
        (None, [only]) => Ok(only),
        (None, []) => bail!("carillon.toml enables no interface"),
        (None, _) => bail!(
            "name the channel's interface with --interface, one of: {}",
            ids()
        ),
    }
}

/// The routed channels of each enabled interface provider, or of `named`
/// alone, one line each under their interface's ID. An interface that does
/// not list its channels says so in their place; when none lists any, that
/// is the failure.
async fn channels(
    config: &LoadedConfig,
    named: Option<&str>,
    operator: &Operator,
) -> Result<String> {
    let providers: Vec<&Provider> = match named {
        Some(_) => vec![addressed(config, named)?],
        None => interfaces(config).collect(),
    };
    let mut text = String::new();
    let mut failures = Vec::new();
    for provider in &providers {
        let url = endpoint(provider, &["v1", "channels"])?;
        let listed = match operator.send(operator.client.get(url)).await {
            Ok(response) => answer::<Vec<Page>>(response).await,
            Err(failure) => Err(failure),
        };
        let _ = writeln!(text, "{}", provider.id);
        match listed {
            Ok(pages) => text.push_str(&summary(&pages)),
            Err(failure) => {
                let failure = format!("{} did not list its channels: {failure:#}", provider.id);
                let _ = writeln!(text, "  ⚠ {failure}");
                failures.push(failure);
            }
        }
    }
    if !failures.is_empty() && failures.len() == providers.len() {
        bail!("{}", failures.join("\n"));
    }
    Ok(text)
}

/// One indented line per page: its subject's ID, its title and the values
/// that apply there, then why its runtime's lines are missing, if they are.
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
            "  {:<id_width$}{:<title_width$}{}",
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
        Some(Subject::Channel { channel_id, .. }) => channel_id,
        Some(Subject::Agent { agent_id }) => agent_id,
        None => "",
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
