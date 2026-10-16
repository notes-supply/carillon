//! `/model`: show the effective model for the conversation and its channel,
//! and change either scope through Discord components. Every change is
//! applied immediately and the panel re-renders from the runtime's confirmed
//! state. A change the runtime refuses, or does not confirm, is reported
//! above the state read afterwards, with controls bound to it; it is never
//! retried. The module keeps nothing between interactions, so the displayed
//! scope and page travel in component custom IDs.
//!
//! Select values and custom ID arguments are fingerprints of catalogue
//! identifiers, never the identifiers themselves: they stay within Discord's
//! length limits whatever the runtime calls its models, and a click resolves
//! against the runtime's current catalogue, so a control left over from an
//! earlier catalogue is reported rather than misapplied. Browsing a provider
//! only filters the model list; choosing a model applies it. Models are paged
//! 25 at a time within the browsed provider. Each setting gets a select while
//! rows allow, otherwise one select picks the setting and another its value.
//! A setting with more than 25 choices, or a model with more than 25
//! settings, shows the first 25.

use carillon_protocol::model::ModelScope as Scope;
use carillon_protocol::models::{AvailableModel, ModelOption, ModelState, ScopeState, Selection};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use serde_json::{Value, json};

use crate::{
    HostCalls,
    components::{PAGE, PRIMARY, SECONDARY, button, choice, fingerprint, select, shorten, text},
    contract::{Interaction, InteractionKind, Outcome, Reply},
};

pub const COMMAND: &str = "model";
/// Provider, model, and action buttons leave two of Discord's five rows.
const SETTING_ROWS: usize = 2;
/// Bound for a runtime's explanation shown above the panel, which leaves
/// room for both scopes' summaries within Discord's message limit.
const NOTE_LIMIT: usize = 400;
const STALE_PROVIDER: &str = "That provider is no longer available; the list below is current.";
const STALE_MODEL: &str = "That model is no longer available; the list below is current.";
const STALE_SETTING: &str = "That setting is no longer available; the controls below are current.";

pub fn definition() -> Value {
    json!({
        "type": 1,
        "name": COMMAND,
        "description": "Show or change the model for this conversation or this channel's default",
    })
}

pub fn interact(host: &mut impl HostCalls, interaction: &Interaction) -> Outcome {
    let action = match interaction.kind {
        InteractionKind::Command if interaction.name == COMMAND => Action::Show(View::default()),
        InteractionKind::Command => {
            return Outcome::Failure(format!("unknown command {}", interaction.name));
        }
        InteractionKind::Component => match parse_control(&interaction.name, &interaction.values) {
            Some(action) => action,
            None => return Outcome::Failure(format!("unknown control {}", interaction.name)),
        },
    };
    let mut view = View {
        scope: action.scope(),
        ..View::default()
    };
    let state = match action {
        Action::Show(shown) => {
            view = shown;
            host.model_state().map_err(Unavailable::Read)
        }
        Action::Default { scope } => host
            .set_model(scope, None, None)
            .map_err(Unavailable::Write),
        Action::ChooseModel { scope, model } => {
            host.model_state()
                .map_err(Unavailable::Read)
                .and_then(|state| {
                    let chosen = state
                        .available
                        .iter()
                        .find(|candidate| model_key(candidate) == model);
                    // A new model starts from its own defaults; the runtime
                    // reports them and the settings rows show them.
                    if let Some(model) = chosen {
                        host.set_model(
                            scope,
                            Some(Selection {
                                provider: model.provider.clone(),
                                model: model.model.clone(),
                                options: BTreeMap::new(),
                            }),
                            None,
                        )
                        .map_err(Unavailable::Write)
                    } else {
                        view.note = Some(STALE_MODEL.to_owned());
                        Ok(state)
                    }
                })
        }
        Action::ChooseOption {
            scope,
            model,
            option,
            choice,
        } => host
            .model_state()
            .map_err(Unavailable::Read)
            .and_then(|state| {
                if let Some(selection) = next_selection(&state, scope, &model, &option, &choice) {
                    view.setting = Some(option);
                    host.set_model(
                        scope,
                        Some(selection),
                        effective(&state, scope).and_then(|scope| scope.selection.clone()),
                    )
                    .map_err(Unavailable::Write)
                } else {
                    view.note = Some(STALE_SETTING.to_owned());
                    Ok(state)
                }
            }),
    };
    let state = match settle(host, &mut view, state) {
        Ok(state) => state,
        Err(outcome) => return outcome,
    };
    let scope = view
        .scope
        .filter(|scope| *scope == Scope::Agent || state.conversation.is_some())
        .unwrap_or(
            if interaction.conversation && state.conversation.is_some() {
                Scope::Conversation
            } else {
                Scope::Agent
            },
        );
    Outcome::Reply(render(&state, scope, &view))
}

/// The state to render, or the reply to send when there is none. The
/// runtime's answer to a refused or unconfirmed change is shown over its
/// current state, read once more; the change is not retried.
fn settle(
    host: &mut impl HostCalls,
    view: &mut View,
    state: Result<ModelState, Unavailable>,
) -> Result<ModelState, Outcome> {
    match state {
        Ok(state) => Ok(state),
        Err(Unavailable::Write(refusal)) => match host.model_state() {
            Ok(state) => {
                view.note = Some(format!(
                    "Your change was not confirmed: {} The settings below are current.",
                    shorten(&refusal, NOTE_LIMIT)
                ));
                Ok(state)
            }
            Err(error) => Err(unavailable(&format!(
                "Your change was not confirmed: {} Model settings are unavailable: {}",
                shorten(&refusal, NOTE_LIMIT),
                shorten(&error, NOTE_LIMIT)
            ))),
        },
        Err(Unavailable::Read(error)) => Err(unavailable(&format!(
            "Model settings are unavailable: {}",
            shorten(&error, 1700)
        ))),
    }
}

/// Why the panel could not be rendered from a confirmed state.
enum Unavailable {
    Read(String),
    /// A change was refused, or its outcome is unknown.
    Write(String),
}

/// A reply without controls, telling the user how to continue.
fn unavailable(explanation: &str) -> Outcome {
    Outcome::Reply(Reply {
        content: format!("{explanation} Reopen /model to refresh."),
        embeds: Vec::new(),
        components: Vec::new(),
    })
}

enum Action {
    /// Re-render without changing anything.
    Show(View),
    /// Return the scope to its default.
    Default { scope: Scope },
    /// Select the model with this fingerprint.
    ChooseModel { scope: Scope, model: String },
    /// Set one setting of the displayed model, identified by fingerprints.
    ChooseOption {
        scope: Scope,
        model: String,
        option: String,
        choice: String,
    },
}

impl Action {
    const fn scope(&self) -> Option<Scope> {
        match self {
            Self::Show(view) => view.scope,
            Self::Default { scope }
            | Self::ChooseModel { scope, .. }
            | Self::ChooseOption { scope, .. } => Some(*scope),
        }
    }
}

/// What the re-rendered panel shows beyond the runtime's state.
#[derive(Default)]
struct View {
    scope: Option<Scope>,
    /// Browsed provider fingerprint; otherwise the scope's active provider.
    provider: Option<String>,
    /// Provider-menu navigation does not change the browsed provider.
    provider_page: Option<usize>,
    /// The model page to show; otherwise the page holding the selection.
    page: Option<usize>,
    /// The setting (by fingerprint) whose choices to show when settings
    /// outnumber their rows.
    setting: Option<String>,
    note: Option<String>,
}

/// Custom IDs follow `model:<control>:<scope>[:<argument>]`, where the
/// argument is a page number or a fingerprint.
fn parse_control(custom_id: &str, values: &[String]) -> Option<Action> {
    let mut parts = custom_id.splitn(4, ':');
    if parts.next()? != COMMAND {
        return None;
    }
    let control = parts.next()?;
    let scope = match parts.next()? {
        "agent" => Scope::Agent,
        "conversation" => Scope::Conversation,
        _ => return None,
    };
    let argument = parts.next();
    let value = values.first();
    let mut view = View {
        scope: Some(scope),
        ..View::default()
    };
    match (control, argument, value) {
        ("scope", None, None) => {}
        ("provider", Some(provider), Some(value)) => {
            if let Some(page) = value.strip_prefix("page:") {
                view.provider = Some(provider.to_owned());
                view.provider_page = Some(page.parse().ok()?);
            } else {
                view.provider = Some(value.clone());
            }
        }
        ("page", Some(argument), None) => {
            let (provider, page) = argument.split_once(':')?;
            view.provider = Some(provider.to_owned());
            view.page = Some(page.parse().ok()?);
        }
        ("setting", None, Some(setting)) => view.setting = Some(setting.clone()),
        ("model", None, Some(model)) => {
            return Some(Action::ChooseModel {
                scope,
                model: model.clone(),
            });
        }
        ("option", Some(argument), Some(choice)) => {
            let (model, option) = argument.split_once(':')?;
            return Some(Action::ChooseOption {
                scope,
                model: model.to_owned(),
                option: option.to_owned(),
                choice: choice.clone(),
            });
        }
        ("default", None, None) => return Some(Action::Default { scope }),
        _ => return None,
    }
    Some(Action::Show(view))
}

/// The selection after changing one setting of the effective model.
/// The other settings keep their currently resolved values.
/// `None` when the model, setting, or choice is not in the catalogue.
fn next_selection(
    state: &ModelState,
    scope: Scope,
    displayed_model: &str,
    option: &str,
    choice: &str,
) -> Option<Selection> {
    let selection = effective(state, scope)?.selection.as_ref()?;
    let model = find_model(state, &selection.provider, &selection.model)?;
    if model_key(model) != displayed_model {
        return None;
    }
    let setting = model
        .options
        .iter()
        .find(|candidate| fingerprint(&[&candidate.id]) == option)?;
    let chosen = setting
        .choices
        .iter()
        .find(|candidate| fingerprint(&[&candidate.id]) == choice)?;
    let mut options = selection.options.clone();
    options.insert(setting.id.clone(), chosen.id.clone());
    Some(Selection {
        provider: selection.provider.clone(),
        model: selection.model.clone(),
        options,
    })
}

fn effective(state: &ModelState, scope: Scope) -> Option<&ScopeState> {
    match scope {
        Scope::Agent => Some(&state.agent),
        Scope::Conversation => state.conversation.as_ref(),
    }
}

fn find_model<'a>(
    state: &'a ModelState,
    provider: &str,
    model: &str,
) -> Option<&'a AvailableModel> {
    state
        .available
        .iter()
        .find(|candidate| candidate.provider == provider && candidate.model == model)
}

fn render(state: &ModelState, scope: Scope, view: &View) -> Reply {
    let mut content = summary(state);
    if let Some(note) = &view.note {
        content.push_str(note);
        content.push('\n');
    }
    let components = if state.available.is_empty() {
        content.push_str("No models are available from the runtime.");
        Vec::new()
    } else if effective(state, scope).is_some_and(|scope| scope.read_only) {
        let _ = write!(
            content,
            "The {} is set by the runtime and cannot be changed here.",
            scope_label(scope)
        );
        let buttons = scope_buttons(state, scope);
        if buttons.is_empty() {
            Vec::new()
        } else {
            vec![json!({ "type": 1, "components": buttons })]
        }
    } else {
        let providers: Vec<&str> = state
            .available
            .iter()
            .map(|model| model.provider.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let browsed = providers
            .iter()
            .find(|provider| Some(fingerprint(&[provider])) == view.provider);
        if view.provider.is_some() && browsed.is_none() {
            let _ = writeln!(content, "{STALE_PROVIDER}");
        }
        let selection = effective(state, scope).and_then(|scope| scope.selection.as_ref());
        let provider = browsed
            .or_else(|| {
                providers
                    .iter()
                    .find(|provider| selection.is_some_and(|s| s.provider == **provider))
            })
            .unwrap_or(&providers[0]);
        let _ = writeln!(content, "Changing the {}:", scope_label(scope));
        let _ = write!(
            content,
            "Browsing provider: {}. Choose a model to apply it.",
            text(provider)
        );
        controls(state, scope, view, &providers, provider)
    };
    Reply {
        content,
        embeds: Vec::new(),
        components,
    }
}

fn summary(state: &ModelState) -> String {
    let mut content = String::from("**Model settings**\n");
    if let Some(conversation) = &state.conversation {
        let _ = writeln!(
            content,
            "This conversation: {} ({})",
            describe(state, conversation.selection.as_ref()),
            if conversation.overridden {
                "set for this conversation"
            } else {
                "follows the default for new conversations"
            }
        );
    }
    let _ = writeln!(
        content,
        "Default for new conversations: {} ({})",
        describe(state, state.agent.selection.as_ref()),
        if state.agent.read_only {
            "set by the runtime"
        } else if state.agent.overridden {
            "set with /model"
        } else {
            "runtime default"
        }
    );
    if state.conversation.is_none() {
        content.push_str("Use `/model` inside a conversation to set a model for it.\n");
    }
    content
}

/// The scope as the panel names it. Every channel routed to an agent shares
/// its default, so the panel never calls it a channel's.
const fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Agent => "default for new conversations",
        Scope::Conversation => "conversation's model",
    }
}

const fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Agent => "agent",
        Scope::Conversation => "conversation",
    }
}

/// Provider and model menus, the active model's settings when browsing its
/// provider, and scope/paging/reset buttons. Browsing never changes a selection.
fn controls(
    state: &ModelState,
    scope: Scope,
    view: &View,
    providers: &[&str],
    provider: &str,
) -> Vec<Value> {
    let name = scope_name(scope);
    let current = effective(state, scope);
    let selection = current.and_then(|scope| scope.selection.as_ref());
    let mut models: Vec<&AvailableModel> = state
        .available
        .iter()
        .filter(|model| model.provider == provider)
        .collect();
    models.sort_by(|left, right| {
        (&left.provider, display_name(left), &left.model).cmp(&(
            &right.provider,
            display_name(right),
            &right.model,
        ))
    });
    let selected = selection.and_then(|selection| {
        models.iter().position(|model| {
            model.provider == selection.provider && model.model == selection.model
        })
    });
    let pages = models.len().div_ceil(PAGE).max(1);
    let page = view
        .page
        .unwrap_or_else(|| selected.map_or(0, |index| index / PAGE))
        .min(pages - 1);
    let mut rows = vec![
        provider_select(name, providers, provider, view.provider_page),
        select(
            &format!("model:model:{name}"),
            &if pages > 1 {
                format!(
                    "Model — showing {}–{} of {}",
                    page * PAGE + 1,
                    ((page + 1) * PAGE).min(models.len()),
                    models.len()
                )
            } else {
                "Model".to_owned()
            },
            models
                .iter()
                .enumerate()
                .skip(page * PAGE)
                .take(PAGE)
                .map(|(index, model)| {
                    choice(
                        display_name(model),
                        &model_key(model),
                        Some(&format!("{}/{}", model.provider, model.model)),
                        selected == Some(index),
                    )
                }),
        ),
    ];
    if let Some((selection, model)) = selection
        .filter(|selection| selection.provider == provider)
        .and_then(|selection| {
            Some((
                selection,
                find_model(state, &selection.provider, &selection.model)?,
            ))
        })
    {
        rows.extend(setting_rows(
            name,
            model,
            selection,
            view.setting.as_deref(),
        ));
    }
    rows.push(actions(state, scope, provider, page, pages));
    rows
}

fn actions(state: &ModelState, scope: Scope, provider: &str, page: usize, pages: usize) -> Value {
    let name = scope_name(scope);
    let current = effective(state, scope);
    let provider_key = fingerprint(&[provider]);
    let mut buttons = scope_buttons(state, scope);
    if pages > 1 {
        buttons.push(button(
            &format!(
                "model:page:{name}:{provider_key}:{}",
                page.saturating_sub(1)
            ),
            "◀ Previous",
            SECONDARY,
            page == 0,
        ));
        buttons.push(button(
            &format!("model:page:{name}:{provider_key}:{}", page + 1),
            "Next ▶",
            SECONDARY,
            page + 1 == pages,
        ));
    }
    buttons.push(button(
        &format!("model:default:{name}"),
        match scope {
            Scope::Agent => "Use runtime default",
            Scope::Conversation => "Use default for new conversations",
        },
        SECONDARY,
        !current.is_some_and(|scope| scope.overridden),
    ));
    json!({ "type": 1, "components": buttons })
}

/// Buttons switching between the conversation and agent scopes, when the
/// panel was opened in a conversation.
fn scope_buttons(state: &ModelState, scope: Scope) -> Vec<Value> {
    if state.conversation.is_none() {
        return Vec::new();
    }
    [
        (Scope::Conversation, "This conversation"),
        (Scope::Agent, "Default for new conversations"),
    ]
    .into_iter()
    .map(|(target, label)| {
        button(
            &format!("model:scope:{}", scope_name(target)),
            label,
            if target == scope { PRIMARY } else { SECONDARY },
            false,
        )
    })
    .collect()
}

/// Rare provider overflow uses navigation choices in the same menu, leaving
/// the normal action buttons and both settings rows available.
fn provider_select(
    scope: &str,
    providers: &[&str],
    provider: &str,
    requested_page: Option<usize>,
) -> Value {
    let size = if providers.len() > PAGE {
        PAGE - 2
    } else {
        PAGE
    };
    let pages = providers.len().div_ceil(size);
    let selected = providers
        .iter()
        .position(|candidate| *candidate == provider)
        .unwrap_or(0);
    let page = requested_page.unwrap_or(selected / size).min(pages - 1);
    let mut options: Vec<Value> = providers
        .iter()
        .skip(page * size)
        .take(size)
        .map(|candidate| {
            choice(
                candidate,
                &fingerprint(&[candidate]),
                None,
                *candidate == provider,
            )
        })
        .collect();
    if page > 0 {
        options.push(choice(
            "◀ Previous providers",
            &format!("page:{}", page - 1),
            None,
            false,
        ));
    }
    if page + 1 < pages {
        options.push(choice(
            "Next providers ▶",
            &format!("page:{}", page + 1),
            None,
            false,
        ));
    }
    select(
        &format!("model:provider:{scope}:{}", fingerprint(&[provider])),
        &if pages > 1 {
            format!("Provider (page {} of {pages})", page + 1)
        } else {
            "Provider".to_owned()
        },
        options.into_iter(),
    )
}

fn setting_rows(
    scope_name: &str,
    model: &AvailableModel,
    selection: &Selection,
    focus: Option<&str>,
) -> Vec<Value> {
    if model.options.len() <= SETTING_ROWS {
        return model
            .options
            .iter()
            .map(|option| choice_select(scope_name, option, selection))
            .collect();
    }
    let Some(focused) = model
        .options
        .iter()
        .find(|option| Some(fingerprint(&[&option.id]).as_str()) == focus)
        .or(model.options.first())
    else {
        return Vec::new();
    };
    vec![
        select(
            &format!("model:setting:{scope_name}"),
            "Setting",
            model.options.iter().map(|option| {
                choice(
                    &option.name,
                    &fingerprint(&[&option.id]),
                    None,
                    option.id == focused.id,
                )
            }),
        ),
        choice_select(scope_name, focused, selection),
    ]
}

fn choice_select(scope_name: &str, option: &ModelOption, selection: &Selection) -> Value {
    let chosen = selection.options.get(&option.id);
    select(
        &format!(
            "model:option:{scope_name}:{}:{}",
            fingerprint(&[&selection.provider, &selection.model]),
            fingerprint(&[&option.id])
        ),
        &option.name,
        option.choices.iter().map(|value| {
            choice(
                &value.id,
                &fingerprint(&[&value.id]),
                None,
                chosen == Some(&value.id),
            )
        }),
    )
}

fn describe(state: &ModelState, selection: Option<&Selection>) -> String {
    let Some(selection) = selection else {
        return "no model selected".to_owned();
    };
    let model = find_model(state, &selection.provider, &selection.model);
    let mut text = match model {
        Some(model) => format!(
            "{} (`{}/{}`)",
            display_name(model),
            model.provider,
            model.model
        ),
        None => format!(
            "`{}/{}` (not currently available)",
            selection.provider, selection.model
        ),
    };
    for (id, value) in &selection.options {
        let name = model
            .and_then(|model| model.options.iter().find(|option| option.id == *id))
            .map_or(id.as_str(), |option| option.name.as_str());
        let _ = write!(text, ", {name}: {value}");
    }
    shorten(&text, 600)
}

fn display_name(model: &AvailableModel) -> &str {
    if model.name.is_empty() {
        &model.model
    } else {
        &model.name
    }
}

/// The select value standing for a model.
fn model_key(model: &AvailableModel) -> String {
    fingerprint(&[&model.provider, &model.model])
}

#[cfg(test)]
mod tests;
