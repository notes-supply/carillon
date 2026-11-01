//! `/settings`: the page of the routed channel the command runs in, also
//! inside one of its threads, with a picker of its editable lines, a picker
//! of the picked line's choices, and `Reset` while the channel sets the
//! picked line itself. Every invocation and click draws from a fresh read and
//! every change applies at once, so the module keeps nothing: the picked
//! line, the revision it was drawn with and each picker's page travel in
//! custom IDs.
//!
//! Custom IDs and select values carry fingerprints of setting and choice IDs,
//! never the IDs themselves, so they stay within Discord's limits whatever
//! the owner calls its settings. A click whose revision is no longer the
//! line's sends nothing and redraws as changed meanwhile.

use std::fmt::Write as _;

use carillon_protocol::settings::{
    Change, Page, Revision, RuntimePart, Setting, Value as SettingValue,
};
use serde_json::{Value, json};

use crate::{
    HostCalls,
    components::{
        NEXT, PREVIOUS, SECONDARY, branded, button, choice, fingerprint, page_holding, paged,
        select, shorten,
    },
    contract::{ActivityMarkers, Interaction, InteractionKind, Notice, Outcome, Reply},
    embed::escaped,
};

pub const COMMAND: &str = "settings";
/// The runtime's model line, whose choices show the operator's model badges.
const MODEL: &str = "model";
/// Discord's limit for a message's text.
const CONTENT_LIMIT: usize = 2000;
/// Bound for a warning above the page, such as a runtime's refusal.
const WARNING_LIMIT: usize = 300;

pub fn definition() -> Value {
    json!({
        "type": 1,
        "name": COMMAND,
        "description": "Show or change this channel's settings",
    })
}

pub fn interact(host: &mut impl HostCalls, interaction: &Interaction) -> Outcome {
    let action = match interaction.kind {
        InteractionKind::Command if interaction.name == COMMAND => Action::Show(View::default()),
        InteractionKind::Command => {
            return Outcome::Failure(format!("unknown command {}", interaction.name));
        }
        InteractionKind::Component => {
            match parse(
                &interaction.name,
                interaction.values.first().map(String::as_str),
            ) {
                Some(action) => action,
                None => return Outcome::Failure(format!("unknown control {}", interaction.name)),
            }
        }
    };
    let drawn = match action {
        Action::Show(view) => host.settings().map(|(page, notice)| (page, notice, view)),
        Action::Change {
            line,
            expected,
            choice,
        } => change(host, line, expected, choice.as_deref()),
    };
    Outcome::Reply(match drawn {
        Ok((page, notice, view)) => render(&page, notice.as_ref(), &view, &interaction.markers),
        Err(reason) => Reply {
            content: warning(&reason),
            embeds: Vec::new(),
            components: Vec::new(),
        },
    })
}

enum Action {
    Show(View),
    /// Set the line with this fingerprint to the choice with this
    /// fingerprint, or reset it without one, if it is still at `expected`.
    Change {
        line: String,
        expected: Revision,
        choice: Option<String>,
    },
}

/// What the pickers show beyond the page.
#[derive(Default)]
struct View {
    /// The picked line's fingerprint; otherwise the first editable line.
    line: Option<String>,
    /// The page each picker shows; otherwise the page holding its pick.
    lines_page: Option<usize>,
    values_page: Option<usize>,
}

/// Custom IDs are `settings:pick:<line>:<page>` for the line picker,
/// `settings:set:<line>:<revision>:<page>` for the picked line's choices and
/// `settings:reset:<line>:<revision>`, where `<line>` is the picked line's
/// fingerprint. A picker's [`PREVIOUS`] and [`NEXT`] values turn its page.
fn parse(custom_id: &str, value: Option<&str>) -> Option<Action> {
    let mut parts = custom_id.split(':');
    if parts.next()? != COMMAND {
        return None;
    }
    let control = parts.next()?;
    let line = parts.next()?.to_owned();
    let action = match (control, value) {
        ("pick", Some(value)) => {
            let page: usize = parts.next()?.parse().ok()?;
            let turned = |lines_page| View {
                line: Some(line.clone()),
                lines_page: Some(lines_page),
                values_page: None,
            };
            Action::Show(match value {
                PREVIOUS => turned(page.saturating_sub(1)),
                NEXT => turned(page + 1),
                picked => View {
                    line: Some(picked.to_owned()),
                    ..View::default()
                },
            })
        }
        ("set", Some(value)) => {
            let expected = Revision::try_from(parts.next()?.to_owned()).ok()?;
            let page: usize = parts.next()?.parse().ok()?;
            let turned = |values_page| {
                Action::Show(View {
                    line: Some(line.clone()),
                    lines_page: None,
                    values_page: Some(values_page),
                })
            };
            match value {
                PREVIOUS => turned(page.saturating_sub(1)),
                NEXT => turned(page + 1),
                choice => Action::Change {
                    line,
                    expected,
                    choice: Some(choice.to_owned()),
                },
            }
        }
        ("reset", None) => Action::Change {
            line,
            expected: Revision::try_from(parts.next()?.to_owned()).ok()?,
            choice: None,
        },
        _ => return None,
    };
    parts.next().is_none().then_some(action)
}

/// Reads the page and, when the clicked line is still as it was drawn,
/// changes it to the clicked choice; otherwise sends nothing.
fn change(
    host: &mut impl HostCalls,
    line: String,
    expected: Revision,
    choice: Option<&str>,
) -> Result<(Page, Option<Notice>, View), String> {
    let (page, _) = host.settings()?;
    let drawn = page
        .settings()
        .find(|setting| fingerprint(&[&setting.id]) == line && setting.revision == expected);
    let change = drawn.and_then(|setting| {
        let value = match choice {
            Some(choice) => Some(SettingValue::One {
                id: setting
                    .choices
                    .iter()
                    .find(|candidate| fingerprint(&[&candidate.id]) == choice)?
                    .id
                    .clone(),
            }),
            None => None,
        };
        Some(Change {
            setting: setting.id.clone(),
            value,
            expected,
        })
    });
    let Some(change) = change else {
        // A runtime that did not answer leaves its lines off the page, which
        // says why; nobody changed the line.
        let outage = matches!(page.runtime, RuntimePart::Unavailable { .. })
            && !page
                .settings()
                .any(|setting| fingerprint(&[&setting.id]) == line);
        let view = View {
            line: Some(line),
            ..View::default()
        };
        return Ok((page, (!outage).then_some(Notice::Stale), view));
    };
    let view = View {
        line: Some(line),
        ..View::default()
    };
    let (page, notice) = host.change_setting(change)?;
    Ok((page, notice, view))
}

fn render(page: &Page, notice: Option<&Notice>, view: &View, markers: &ActivityMarkers) -> Reply {
    let mut warnings: Vec<&str> = notice
        .map(|notice| match notice {
            Notice::Stale => "Changed meanwhile",
            Notice::Unconfirmed => "Not confirmed",
            Notice::Refused(reason) => reason,
        })
        .into_iter()
        .collect();
    if let RuntimePart::Unavailable { reason } = &page.runtime
        && !warnings.contains(&reason.as_str())
    {
        warnings.push(reason);
    }
    Reply {
        content: content(page, &warnings),
        embeds: Vec::new(),
        components: controls(page, view, markers),
    }
}

/// Text that can neither format nor mention anything.
fn warning(text: &str) -> String {
    format!("⚠ {}", escaped(&shorten(text, WARNING_LIMIT)))
}

/// The warnings, the title and the page's lines. Discord draws text in a
/// proportional font, so the lines go in a code block, where the padded
/// names align; the title stays outside it, where Discord shows a channel
/// mention as the channel's name. A page beyond Discord's limit ends with
/// `…` where it was cut.
fn content(page: &Page, warnings: &[&str]) -> String {
    let mut text = String::new();
    for shown in warnings {
        let _ = writeln!(text, "{}", warning(shown));
    }
    let _ = write!(text, "{}\n```\n", page.title);
    let mut length = text.chars().count();
    let room = CONTENT_LIMIT.saturating_sub("…\n```".chars().count());
    for line in page.lines() {
        let line = line.replace('`', "ˋ");
        length += line.chars().count() + 1;
        if length > room {
            text.push_str("…\n");
            break;
        }
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str("```");
    text
}

fn holds(setting: &Setting, id: &str) -> bool {
    match &setting.value {
        Some(SettingValue::One { id: held }) => held == id,
        Some(SettingValue::Many { ids }) => ids.iter().any(|held| held == id),
        None => false,
    }
}

fn editable(setting: &Setting) -> bool {
    setting
        .choices
        .iter()
        .any(|choice| !holds(setting, &choice.id))
}

fn controls(page: &Page, view: &View, markers: &ActivityMarkers) -> Vec<Value> {
    let editable: Vec<&Setting> = page
        .settings()
        .filter(|setting| editable(setting))
        .collect();
    let index = view
        .line
        .as_deref()
        .and_then(|line| {
            editable
                .iter()
                .position(|setting| fingerprint(&[&setting.id]) == line)
        })
        .unwrap_or_default();
    let Some(picked) = editable.get(index) else {
        return Vec::new();
    };
    let line = fingerprint(&[&picked.id]);
    let (lines_page, lines) = paged(
        editable
            .iter()
            .map(|setting| {
                choice(
                    &setting.name,
                    &fingerprint(&[&setting.id]),
                    None,
                    setting.id == picked.id,
                )
            })
            .collect(),
        view.lines_page
            .unwrap_or_else(|| page_holding(index, editable.len())),
    );
    let held = picked
        .choices
        .iter()
        .position(|candidate| holds(picked, &candidate.id));
    let (values_page, values) = paged(
        picked
            .choices
            .iter()
            .map(|candidate| {
                let option = choice(
                    &candidate.name,
                    &fingerprint(&[&candidate.id]),
                    candidate.group.as_deref(),
                    holds(picked, &candidate.id),
                );
                if picked.id == MODEL {
                    // Model choices are `<provider>/<model>`; badges match the model.
                    let model = candidate
                        .id
                        .split_once('/')
                        .map_or(candidate.id.as_str(), |(_, model)| model);
                    branded(option, markers.brand(model))
                } else {
                    option
                }
            })
            .collect(),
        view.values_page
            .unwrap_or_else(|| held.map_or(0, |index| page_holding(index, picked.choices.len()))),
    );
    let mut rows = vec![
        select(
            &format!("{COMMAND}:pick:{line}:{lines_page}"),
            "Setting",
            lines.into_iter(),
        ),
        select(
            &format!("{COMMAND}:set:{line}:{}:{values_page}", picked.revision),
            &picked.name,
            values.into_iter(),
        ),
    ];
    if picked.value.is_some() && !picked.inherited {
        rows.push(json!({ "type": 1, "components": [button(
            &format!("{COMMAND}:reset:{line}:{}", picked.revision),
            "Reset",
            SECONDARY,
            false,
        )]}));
    }
    rows
}

#[cfg(test)]
mod tests;
