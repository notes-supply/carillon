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
        .find(|candidate| fingerprint(&[candidate]) == choice)?;
    let mut options = selection.options.clone();
    options.insert(setting.id.clone(), chosen.clone());
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
        option
            .choices
            .iter()
            .map(|value| choice(value, &fingerprint(&[value]), None, chosen == Some(value))),
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
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::{COMMAND, interact};
    use crate::components::{PAGE, TEXT_LIMIT, fingerprint};
    use crate::{
        HostCalls,
        contract::{HostReply, HostRequest, Interaction, InteractionKind, Outcome, Reply},
    };
    use carillon_protocol::{
        model::ModelScope as Scope,
        models::{AvailableModel, ModelOption, ModelState, ScopeState, Selection},
    };

    /// Answers like the runtime: a set selection is resolved so every
    /// supported setting has a value, the first choice standing for the
    /// runtime's default.
    struct FakeRuntime {
        state: ModelState,
        requests: Vec<HostRequest>,
        failure: Option<String>,
        /// Refuse every change with this reason, as a runtime does when the
        /// scope changed since the panel was read.
        refusal: Option<String>,
        /// The other writer's selection the conversation holds after a refusal.
        concurrent: Option<Selection>,
        /// Fail every call after the first refusal.
        then_fail: Option<String>,
    }

    impl HostCalls for FakeRuntime {
        fn call(&mut self, request: HostRequest) -> Result<HostReply, String> {
            if let Some(failure) = &self.failure {
                return Err(failure.clone());
            }
            if let (HostRequest::SetModel { .. }, Some(refusal)) = (&request, &self.refusal) {
                let refusal = refusal.clone();
                if let (Some(selection), Some(conversation)) =
                    (self.concurrent.take(), self.state.conversation.as_mut())
                {
                    *conversation = ScopeState {
                        selection: Some(selection),
                        overridden: true,
                        read_only: false,
                    };
                }
                self.failure = self.then_fail.take();
                self.requests.push(request);
                return Err(refusal);
            }
            if let HostRequest::SetModel {
                scope, selection, ..
            } = &request
            {
                let agent_default = self.state.agent.selection.clone();
                let mut resolved = None;
                if let Some(selection) = selection {
                    let model = self
                        .state
                        .available
                        .iter()
                        .find(|candidate| {
                            candidate.provider == selection.provider
                                && candidate.model == selection.model
                        })
                        .ok_or("That model is not available.")?;
                    let mut options = BTreeMap::new();
                    for option in &model.options {
                        let chosen = selection
                            .options
                            .get(&option.id)
                            .filter(|chosen| option.choices.contains(chosen))
                            .or(option.choices.first())
                            .ok_or("a setting without choices")?;
                        options.insert(option.id.clone(), chosen.clone());
                    }
                    resolved = Some(Selection {
                        provider: selection.provider.clone(),
                        model: selection.model.clone(),
                        options,
                    });
                }
                let target = match scope {
                    Scope::Agent => &mut self.state.agent,
                    Scope::Conversation => {
                        self.state.conversation.as_mut().ok_or("no conversation")?
                    }
                };
                *target = match resolved {
                    Some(selection) => ScopeState {
                        selection: Some(selection),
                        overridden: true,
                        read_only: false,
                    },
                    None => ScopeState {
                        selection: agent_default,
                        overridden: false,
                        read_only: false,
                    },
                };
            }
            self.requests.push(request);
            Ok(HostReply::ModelState(self.state.clone()))
        }
    }

    fn model(provider: &str, id: &str, thinking: &[&str]) -> AvailableModel {
        AvailableModel {
            provider: provider.to_owned(),
            model: id.to_owned(),
            name: format!("{id} name"),
            options: if thinking.is_empty() {
                Vec::new()
            } else {
                vec![ModelOption {
                    id: "thinking".to_owned(),
                    name: "Thinking".to_owned(),
                    choices: thinking.iter().map(|level| (*level).to_owned()).collect(),
                    default: thinking.first().copied().unwrap_or_default().to_owned(),
                }]
            },
        }
    }

    fn selection(provider: &str, id: &str, thinking: Option<&str>) -> Selection {
        Selection {
            provider: provider.to_owned(),
            model: id.to_owned(),
            options: thinking
                .map(|level| BTreeMap::from([("thinking".to_owned(), level.to_owned())]))
                .unwrap_or_default(),
        }
    }

    fn runtime(conversation: bool) -> FakeRuntime {
        let agent = ScopeState {
            selection: Some(selection("anthropic", "sonnet", Some("medium"))),
            overridden: false,
            read_only: false,
        };
        FakeRuntime {
            state: ModelState {
                available: vec![
                    model("anthropic", "sonnet", &["off", "low", "medium", "high"]),
                    model("anthropic", "opus", &["off", "high"]),
                    model("openai", "gpt", &[]),
                ],
                agent: agent.clone(),
                conversation: conversation.then_some(agent),
            },
            requests: Vec::new(),
            failure: None,
            refusal: None,
            concurrent: None,
            then_fail: None,
        }
    }

    #[test]
    fn oversized_state_and_errors_keep_replies_sendable() -> Result<(), String> {
        let mut host = runtime(true);
        let long = "🦀".repeat(3000);
        host.state.available = vec![model(&long, &long, &[&long])];
        let scope = ScopeState {
            selection: Some(selection(&long, &long, Some(&long))),
            overridden: true,
            read_only: false,
        };
        host.state.agent = scope.clone();
        host.state.conversation = Some(scope);
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        assert!(reply.content.contains("This conversation:"));
        assert!(reply.content.contains("Default for new conversations:"));
        assert!(reply.content.contains('…'));
        assert!(!reply.components.is_empty());
        host.refusal = Some(long.clone());
        let reply = expect_reply(interact(
            &mut host,
            &component("model:default:conversation", &[]),
        ))?;
        assert!(reply.content.contains("Your change was not confirmed:"));
        assert!(reply.content.contains("Default for new conversations:"));
        assert!(!reply.components.is_empty());
        host.failure = Some(long);
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        assert!(reply.content.starts_with("Model settings are unavailable:"));
        Ok(())
    }

    #[test]
    fn a_read_only_scope_shows_its_selection_without_controls() -> Result<(), String> {
        let mut host = runtime(true);
        host.state.agent.read_only = true;
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        assert!(
            reply
                .content
                .contains("Default for new conversations: sonnet name")
        );
        assert!(reply.content.contains("(set by the runtime)"));
        assert!(custom_ids(&reply).contains(&"model:model:conversation".to_owned()));

        let reply = expect_reply(interact(&mut host, &component("model:scope:agent", &[])))?;
        assert!(reply.content.ends_with(
            "The default for new conversations is set by the runtime and cannot be \
             changed here."
        ));
        assert_eq!(
            custom_ids(&reply),
            ["model:scope:conversation", "model:scope:agent"]
        );

        let mut host = runtime(false);
        host.state.agent.read_only = true;
        let reply = expect_reply(interact(&mut host, &command(false)))?;
        assert!(reply.components.is_empty());
        assert!(
            host.requests
                .iter()
                .all(|request| matches!(request, HostRequest::ModelState))
        );
        Ok(())
    }

    fn command(conversation: bool) -> Interaction {
        Interaction {
            kind: InteractionKind::Command,
            name: COMMAND.to_owned(),
            values: Vec::new(),
            options: Value::Null,
            conversation,
        }
    }

    fn component(custom_id: &str, values: &[&str]) -> Interaction {
        Interaction {
            kind: InteractionKind::Component,
            name: custom_id.to_owned(),
            values: values.iter().map(|value| (*value).to_owned()).collect(),
            options: Value::Null,
            conversation: true,
        }
    }

    fn key(provider: &str, model: &str) -> String {
        fingerprint(&[provider, model])
    }

    fn option_control(scope: &str, provider: &str, model: &str, option: &str) -> String {
        format!(
            "model:option:{scope}:{}:{}",
            key(provider, model),
            fingerprint(&[option])
        )
    }

    fn provider_control(scope: &str, provider: &str) -> String {
        format!("model:provider:{scope}:{}", fingerprint(&[provider]))
    }

    fn model_page(scope: &str, provider: &str, page: usize) -> String {
        format!("model:page:{scope}:{}:{page}", fingerprint(&[provider]))
    }

    fn thinking_control(scope: &str) -> String {
        option_control(scope, "anthropic", "sonnet", "thinking")
    }

    /// A reply, checked against the Discord component limits the module
    /// promises to respect.
    fn expect_reply(outcome: Outcome) -> Result<Reply, String> {
        let Outcome::Reply(reply) = outcome else {
            return Err(format!("expected a reply, got {outcome:?}"));
        };
        assert!(
            reply.content.chars().count() <= 2000,
            "reply content too long"
        );
        assert!(reply.components.len() <= 5, "too many rows");
        for row in &reply.components {
            let components = row["components"]
                .as_array()
                .ok_or("row without components")?;
            assert!(!components.is_empty() && components.len() <= 5);
            for component in components {
                assert!(
                    component["custom_id"]
                        .as_str()
                        .is_some_and(|id| id.len() <= 100)
                );
                if component["type"] == 2 {
                    assert!(component["label"].as_str().is_some_and(|l| l.len() <= 80));
                    continue;
                }
                assert!(components.len() == 1, "a select fills its row");
                let options = component["options"]
                    .as_array()
                    .ok_or("select without options")?;
                assert!(
                    !options.is_empty() && options.len() <= PAGE,
                    "select option count"
                );
                for option in options {
                    for field in ["label", "value", "description"] {
                        if let Some(text) = option[field].as_str() {
                            assert!(!text.is_empty() && text.chars().count() <= TEXT_LIMIT);
                        }
                    }
                }
            }
        }
        Ok(reply)
    }

    fn components(reply: &Reply) -> Vec<&Value> {
        reply
            .components
            .iter()
            .filter_map(|row| row["components"].as_array())
            .flatten()
            .collect()
    }

    fn custom_ids(reply: &Reply) -> Vec<String> {
        components(reply)
            .into_iter()
            .filter_map(|component| component["custom_id"].as_str())
            .map(str::to_owned)
            .collect()
    }

    fn component_by_id<'a>(reply: &'a Reply, custom_id: &str) -> &'a Value {
        components(reply)
            .into_iter()
            .find(|component| component["custom_id"] == custom_id)
            .unwrap_or(&Value::Null)
    }

    fn selected(reply: &Reply, custom_id: &str) -> Option<String> {
        component_by_id(reply, custom_id)["options"]
            .as_array()?
            .iter()
            .find(|option| option["default"] == true)?["value"]
            .as_str()
            .map(str::to_owned)
    }

    fn values(reply: &Reply, custom_id: &str) -> Vec<String> {
        component_by_id(reply, custom_id)["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| option["value"].as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn command_shows_both_scopes_and_targets_the_conversation_first() -> Result<(), String> {
        let mut host = runtime(true);
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        assert!(reply.content.contains("This conversation: sonnet name"));
        assert!(reply.content.contains("Thinking: medium"));
        assert!(
            reply
                .content
                .contains("follows the default for new conversations")
        );
        assert!(
            reply
                .content
                .contains("Default for new conversations: sonnet name")
        );
        assert_eq!(
            custom_ids(&reply),
            [
                provider_control("conversation", "anthropic"),
                "model:model:conversation".to_owned(),
                thinking_control("conversation"),
                "model:scope:conversation".to_owned(),
                "model:scope:agent".to_owned(),
                "model:default:conversation".to_owned(),
            ]
        );
        assert_eq!(
            selected(&reply, "model:model:conversation"),
            Some(key("anthropic", "sonnet"))
        );
        assert_eq!(
            selected(&reply, &thinking_control("conversation")),
            Some(fingerprint(&["medium"]))
        );
        let model = component_by_id(&reply, "model:model:conversation");
        assert_eq!(model["placeholder"], "Model");
        assert_eq!(model["options"][0]["label"], "opus name");
        assert_eq!(model["options"][0]["description"], "anthropic/opus");
        // The displayed scope is highlighted; nothing to undo while the
        // conversation follows the default for new conversations.
        assert_eq!(
            component_by_id(&reply, "model:scope:conversation")["style"],
            1
        );
        assert_eq!(component_by_id(&reply, "model:scope:agent")["style"], 2);
        assert_eq!(
            component_by_id(&reply, "model:default:conversation")["disabled"],
            true
        );
        assert_eq!(host.requests, [HostRequest::ModelState]);
        Ok(())
    }

    #[test]
    fn outside_a_conversation_only_the_agent_scope_is_offered() -> Result<(), String> {
        let mut host = runtime(false);
        let reply = expect_reply(interact(&mut host, &command(false)))?;
        assert!(reply.content.contains("Use `/model` inside a conversation"));
        assert_eq!(
            custom_ids(&reply),
            [
                provider_control("agent", "anthropic"),
                "model:model:agent".to_owned(),
                thinking_control("agent"),
                "model:default:agent".to_owned(),
            ]
        );
        Ok(())
    }

    #[test]
    fn choosing_a_model_starts_from_its_defaults_and_settings_pin_the_view() -> Result<(), String> {
        let mut host = runtime(true);
        let reply = expect_reply(interact(
            &mut host,
            &component("model:model:conversation", &[&key("openai", "gpt")]),
        ))?;
        assert_eq!(
            host.requests[1],
            HostRequest::SetModel {
                scope: Scope::Conversation,
                selection: Some(selection("openai", "gpt", None)),
                expected: None,
            }
        );
        assert!(reply.content.contains("This conversation: gpt name"));
        assert!(reply.content.contains("set for this conversation"));
        assert!(!custom_ids(&reply).contains(&option_control(
            "conversation",
            "anthropic",
            "opus",
            "thinking"
        )));
        assert_eq!(
            component_by_id(&reply, "model:default:conversation")["disabled"],
            false
        );

        let reply = expect_reply(interact(
            &mut host,
            &component("model:model:conversation", &[&key("anthropic", "opus")]),
        ))?;
        assert_eq!(
            host.requests[3],
            HostRequest::SetModel {
                scope: Scope::Conversation,
                selection: Some(selection("anthropic", "opus", None)),
                expected: None,
            }
        );
        assert_eq!(
            selected(
                &reply,
                &option_control("conversation", "anthropic", "opus", "thinking")
            ),
            Some(fingerprint(&["off"]))
        );
        let reply = expect_reply(interact(
            &mut host,
            &component(
                &option_control("conversation", "anthropic", "opus", "thinking"),
                &[&fingerprint(&["high"])],
            ),
        ))?;
        assert_eq!(
            host.requests[5],
            HostRequest::SetModel {
                scope: Scope::Conversation,
                selection: Some(selection("anthropic", "opus", Some("high"))),
                expected: Some(selection("anthropic", "opus", Some("off"))),
            }
        );
        assert!(reply.content.contains("Thinking: high"));
        Ok(())
    }

    #[test]
    fn provider_browsing_is_read_only_and_model_selection_uses_the_chosen_scope()
    -> Result<(), String> {
        for scope in [Scope::Conversation, Scope::Agent] {
            let mut host = runtime(true);
            let name = super::scope_name(scope);
            let initial = expect_reply(interact(
                &mut host,
                &component(&format!("model:scope:{name}"), &[]),
            ))?;
            assert_eq!(
                selected(&initial, &provider_control(name, "anthropic")),
                Some(fingerprint(&["anthropic"]))
            );
            assert_eq!(
                values(&initial, &format!("model:model:{name}")),
                [key("anthropic", "opus"), key("anthropic", "sonnet")]
            );
            let before = host.state.clone();
            let calls = host.requests.len();
            let browsed = expect_reply(interact(
                &mut host,
                &component(
                    &provider_control(name, "anthropic"),
                    &[&fingerprint(&["openai"])],
                ),
            ))?;
            assert_eq!(host.state, before);
            assert_eq!(&host.requests[calls..], &[HostRequest::ModelState]);
            assert!(browsed.content.contains("This conversation: sonnet name"));
            assert!(
                browsed
                    .content
                    .contains("Default for new conversations: sonnet name")
            );
            assert!(browsed.content.contains("Browsing provider: openai"));
            assert_eq!(
                values(&browsed, &format!("model:model:{name}")),
                [key("openai", "gpt")]
            );
            assert_eq!(selected(&browsed, &format!("model:model:{name}")), None);
            assert!(
                !custom_ids(&browsed)
                    .iter()
                    .any(|id| id.starts_with("model:option:"))
            );
            let applied = expect_reply(interact(
                &mut host,
                &component(&format!("model:model:{name}"), &[&key("openai", "gpt")]),
            ))?;
            assert_eq!(
                host.requests.last(),
                Some(&HostRequest::SetModel {
                    scope,
                    selection: Some(selection("openai", "gpt", None)),
                    expected: None,
                })
            );
            assert_eq!(
                selected(&applied, &provider_control(name, "openai")),
                Some(fingerprint(&["openai"]))
            );
            assert_eq!(
                selected(&applied, &format!("model:model:{name}")),
                Some(key("openai", "gpt"))
            );
            let other_scope = if scope == Scope::Agent {
                Scope::Conversation
            } else {
                Scope::Agent
            };
            assert_eq!(
                super::effective(&host.state, other_scope),
                super::effective(&before, other_scope)
            );
            let other = expect_reply(interact(
                &mut host,
                &component(
                    &format!("model:scope:{}", super::scope_name(other_scope)),
                    &[],
                ),
            ))?;
            assert!(other.content.contains("Browsing provider: anthropic"));
        }
        Ok(())
    }

    #[test]
    fn provider_removal_and_paging_do_not_retarget_a_selection() -> Result<(), String> {
        let mut host = runtime(true);
        for index in 0..60 {
            host.state
                .available
                .push(model("zeta", &format!("model-{index:02}"), &[]));
        }
        let before = host.state.clone();
        let browsed = expect_reply(interact(
            &mut host,
            &component(
                &provider_control("conversation", "anthropic"),
                &[&fingerprint(&["zeta"])],
            ),
        ))?;
        assert!(browsed.content.contains("Browsing provider: zeta"));
        let page = expect_reply(interact(
            &mut host,
            &component(&model_page("conversation", "zeta", 1), &[]),
        ))?;
        assert!(page.content.contains("Browsing provider: zeta"));
        assert_eq!(values(&page, "model:model:conversation").len(), PAGE);
        assert!(values(&page, "model:model:conversation").contains(&key("zeta", "model-25")));
        assert_eq!(host.state, before);
        host.state
            .available
            .retain(|model| model.provider != "zeta");
        for control in [
            component(&model_page("conversation", "zeta", 2), &[]),
            component(
                &provider_control("conversation", "anthropic"),
                &[&fingerprint(&["zeta"])],
            ),
        ] {
            let reply = expect_reply(interact(&mut host, &control))?;
            assert!(
                reply
                    .content
                    .contains("That provider is no longer available")
            );
            assert!(reply.content.contains("Browsing provider: anthropic"));
        }
        assert!(
            host.requests
                .iter()
                .all(|request| *request == HostRequest::ModelState)
        );
        assert_eq!(host.state.conversation, before.conversation);
        Ok(())
    }

    #[test]
    fn provider_overflow_keeps_every_provider_reachable_within_the_row_limit() -> Result<(), String>
    {
        let mut host = runtime(true);
        for index in 0..60 {
            host.state
                .available
                .push(model(&format!("provider-{index:02}"), "model", &[]));
        }
        for index in 0..30 {
            host.state
                .available
                .push(model("anthropic", &format!("model-{index:02}"), &[]));
        }
        host.state.available[0].options = (0..4)
            .map(|index| ModelOption {
                id: format!("setting-{index}"),
                name: format!("Setting {index}"),
                choices: vec!["one".to_owned(), "two".to_owned()],
                default: "one".to_owned(),
            })
            .collect();
        let before = host.state.clone();
        let expected: std::collections::BTreeSet<_> = host
            .state
            .available
            .iter()
            .map(|model| fingerprint(&[&model.provider]))
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let control = provider_control("conversation", "anthropic");
        let mut reply = expect_reply(interact(&mut host, &command(true)))?;
        loop {
            assert_eq!(reply.components.len(), 5);
            let choices = values(&reply, &control);
            let previously_seen = seen.len();
            seen.extend(
                choices
                    .iter()
                    .filter(|value| !value.starts_with("page:"))
                    .cloned(),
            );
            assert!(
                seen.len() > previously_seen,
                "provider paging must make progress"
            );
            let next = component_by_id(&reply, &control)["options"]
                .as_array()
                .ok_or("provider choices")?
                .iter()
                .find(|option| option["label"] == "Next providers ▶")
                .and_then(|option| option["value"].as_str())
                .map(str::to_owned);
            let Some(next) = next else { break };
            reply = expect_reply(interact(&mut host, &component(&control, &[&next])))?;
        }
        assert_eq!(seen, expected);
        assert_eq!(host.state, before);
        assert!(
            host.requests
                .iter()
                .all(|request| *request == HostRequest::ModelState)
        );
        let chosen = "provider-59";
        let reply = expect_reply(interact(
            &mut host,
            &component(&control, &[&fingerprint(&[chosen])]),
        ))?;
        assert_eq!(
            selected(&reply, &provider_control("conversation", chosen)),
            Some(fingerprint(&[chosen]))
        );
        assert_eq!(
            values(&reply, "model:model:conversation"),
            [key(chosen, "model")]
        );
        Ok(())
    }

    #[test]
    fn scope_switching_and_defaults_apply_to_the_displayed_scope() -> Result<(), String> {
        let mut host = runtime(true);
        let reply = expect_reply(interact(&mut host, &component("model:scope:agent", &[])))?;
        assert!(custom_ids(&reply).contains(&"model:default:agent".to_owned()));
        assert_eq!(component_by_id(&reply, "model:scope:agent")["style"], 1);
        let reply = expect_reply(interact(
            &mut host,
            &component("model:model:agent", &[&key("anthropic", "opus")]),
        ))?;
        assert!(
            reply
                .content
                .contains("Default for new conversations: opus name")
        );
        assert!(reply.content.contains("(set with /model)"));
        let reply = expect_reply(interact(&mut host, &component("model:default:agent", &[])))?;
        assert_eq!(
            host.requests[3],
            HostRequest::SetModel {
                scope: Scope::Agent,
                selection: None,
                expected: None,
            }
        );
        assert!(reply.content.contains("(runtime default)"));
        Ok(())
    }

    #[test]
    fn paging_reaches_every_model_and_opens_on_the_selection() -> Result<(), String> {
        let mut host = runtime(true);
        for index in 0..60 {
            host.state
                .available
                .push(model("zeta", &format!("model-{index:02}"), &[]));
        }
        let last = selection("zeta", "model-59", None);
        host.state.conversation = Some(ScopeState {
            selection: Some(last.clone()),
            overridden: true,
            read_only: false,
        });
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        let model = component_by_id(&reply, "model:model:conversation");
        assert_eq!(model["placeholder"], "Model — showing 51–60 of 60");
        assert_eq!(
            selected(&reply, "model:model:conversation"),
            Some(key("zeta", "model-59"))
        );
        assert_eq!(
            component_by_id(&reply, &model_page("conversation", "zeta", 1))["disabled"],
            false
        );
        assert_eq!(
            component_by_id(&reply, &model_page("conversation", "zeta", 3))["disabled"],
            true
        );

        let mut seen = Vec::new();
        for page in 0..3 {
            let reply = expect_reply(interact(
                &mut host,
                &component(&model_page("conversation", "zeta", page), &[]),
            ))?;
            seen.extend(values(&reply, "model:model:conversation"));
        }
        seen.sort_unstable();
        let mut expected: Vec<String> = host
            .state
            .available
            .iter()
            .filter(|model| model.provider == "zeta")
            .map(super::model_key)
            .collect();
        expected.sort_unstable();
        assert_eq!(seen, expected);
        assert_eq!(host.requests.len(), 4);

        // Choosing from another page applies and returns to the selection's page.
        let reply = expect_reply(interact(
            &mut host,
            &component("model:model:conversation", &[&key("zeta", "model-00")]),
        ))?;
        assert_eq!(
            host.requests[5],
            HostRequest::SetModel {
                scope: Scope::Conversation,
                selection: Some(selection("zeta", "model-00", None)),
                expected: None,
            }
        );
        assert_eq!(
            component_by_id(&reply, "model:model:conversation")["placeholder"],
            "Model — showing 1–25 of 60"
        );
        Ok(())
    }

    #[test]
    fn identifiers_of_any_length_round_trip_unchanged() -> Result<(), String> {
        let mut host = runtime(true);
        let long = "m".repeat(150);
        let provider = "p:".repeat(90);
        host.state
            .available
            .push(model(&provider, &long, &["off", "on"]));
        let reply = expect_reply(interact(
            &mut host,
            &component(
                &provider_control("conversation", "anthropic"),
                &[&fingerprint(&[&provider])],
            ),
        ))?;
        let model = component_by_id(&reply, "model:model:conversation");
        let option = model["options"]
            .as_array()
            .and_then(|options| {
                options
                    .iter()
                    .find(|option| option["value"] == key(&provider, &long))
            })
            .ok_or("the long model is offered")?;
        assert_eq!(option["label"].as_str().map(str::len), Some(TEXT_LIMIT));
        assert_eq!(
            option["description"].as_str().map(str::len),
            Some(TEXT_LIMIT)
        );
        interact(
            &mut host,
            &component("model:model:conversation", &[&key(&provider, &long)]),
        );
        assert_eq!(
            host.requests[2],
            HostRequest::SetModel {
                scope: Scope::Conversation,
                selection: Some(selection(&provider, &long, None)),
                expected: None,
            }
        );
        Ok(())
    }

    #[test]
    fn a_setting_from_an_old_panel_never_targets_a_different_model() -> Result<(), String> {
        for scope in [Scope::Agent, Scope::Conversation] {
            let mut host = runtime(true);
            let scope_name = if scope == Scope::Agent {
                "agent"
            } else {
                "conversation"
            };
            let old = expect_reply(interact(
                &mut host,
                &component(&format!("model:scope:{scope_name}"), &[]),
            ))?;
            let id = custom_ids(&old)
                .into_iter()
                .find(|id| id.starts_with("model:option:"))
                .ok_or("thinking control")?;
            let high = component_by_id(&old, &id)["options"]
                .as_array()
                .ok_or("choices")?
                .iter()
                .find(|option| option["label"] == "high")
                .and_then(|option| option["value"].as_str())
                .ok_or("high choice")?;
            // Both models offer the exact same setting/value. Identity, not
            // catalogue membership, must prevent this old control from writing.
            let target = if scope == Scope::Agent {
                &mut host.state.agent
            } else {
                host.state.conversation.as_mut().ok_or("conversation")?
            };
            target.selection = Some(selection("anthropic", "opus", Some("off")));
            let before = host.state.clone();
            let calls = host.requests.len();
            let reply = expect_reply(interact(&mut host, &component(&id, &[high])))?;
            assert!(
                reply
                    .content
                    .contains("That setting is no longer available")
            );
            assert!(reply.content.contains("opus name"));
            assert_eq!(host.state, before);
            assert_eq!(&host.requests[calls..], &[HostRequest::ModelState]);
        }
        Ok(())
    }

    #[test]
    fn stale_controls_are_reported_without_changing_anything() -> Result<(), String> {
        let mut host = runtime(true);
        let reply = expect_reply(interact(
            &mut host,
            &component("model:model:conversation", &[&key("anthropic", "retired")]),
        ))?;
        assert!(reply.content.contains("That model is no longer available"));
        assert!(custom_ids(&reply).contains(&"model:model:conversation".to_owned()));
        let reply = expect_reply(interact(
            &mut host,
            &component(&thinking_control("conversation"), &[&fingerprint(&["max"])]),
        ))?;
        assert!(
            reply
                .content
                .contains("That setting is no longer available")
        );
        let reply = expect_reply(interact(
            &mut host,
            &component(
                &option_control("conversation", "anthropic", "sonnet", "speed"),
                &[&fingerprint(&["fast"])],
            ),
        ))?;
        assert!(
            reply
                .content
                .contains("That setting is no longer available")
        );
        assert_eq!(host.requests, [const { HostRequest::ModelState }; 3]);
        Ok(())
    }

    #[test]
    fn a_selection_missing_from_the_catalogue_leaves_no_empty_control() -> Result<(), String> {
        let mut host = runtime(true);
        host.state.conversation = Some(ScopeState {
            selection: Some(selection("removed", "gone", Some("high"))),
            overridden: true,
            read_only: false,
        });
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        assert!(
            reply
                .content
                .contains("`removed/gone` (not currently available)")
        );
        assert_eq!(
            custom_ids(&reply),
            [
                provider_control("conversation", "anthropic"),
                "model:model:conversation".to_owned(),
                "model:scope:conversation".to_owned(),
                "model:scope:agent".to_owned(),
                "model:default:conversation".to_owned(),
            ]
        );
        assert_eq!(selected(&reply, "model:model:conversation"), None);
        assert_eq!(values(&reply, "model:model:conversation").len(), 2);
        assert_eq!(
            component_by_id(&reply, "model:default:conversation")["disabled"],
            false
        );
        Ok(())
    }

    #[test]
    fn many_settings_are_reached_through_a_setting_chooser() -> Result<(), String> {
        let mut host = runtime(true);
        let options = ["thinking", "speed", "verbosity", "format"];
        host.state.available[0].options = options
            .iter()
            .map(|id| ModelOption {
                id: (*id).to_owned(),
                name: id.to_uppercase(),
                choices: vec!["one".to_owned(), "two".to_owned()],
                default: "one".to_owned(),
            })
            .collect();
        let shown: BTreeMap<String, String> = options
            .iter()
            .map(|id| ((*id).to_owned(), "one".to_owned()))
            .collect();
        host.state.conversation = Some(ScopeState {
            selection: Some(Selection {
                options: shown.clone(),
                ..selection("anthropic", "sonnet", None)
            }),
            overridden: false,
            read_only: false,
        });
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        assert_eq!(
            custom_ids(&reply)[..4],
            [
                provider_control("conversation", "anthropic"),
                "model:model:conversation".to_owned(),
                "model:setting:conversation".to_owned(),
                thinking_control("conversation"),
            ]
        );
        assert_eq!(
            values(&reply, "model:setting:conversation"),
            options
                .iter()
                .map(|id| fingerprint(&[id]))
                .collect::<Vec<_>>()
        );
        let speed = option_control("conversation", "anthropic", "sonnet", "speed");
        let reply = expect_reply(interact(
            &mut host,
            &component("model:setting:conversation", &[&fingerprint(&["speed"])]),
        ))?;
        assert!(custom_ids(&reply).contains(&speed));
        assert_eq!(
            selected(&reply, "model:setting:conversation"),
            Some(fingerprint(&["speed"]))
        );
        // Changing one setting records every displayed setting, so the scope
        // runs exactly what the panel showed.
        let reply = expect_reply(interact(
            &mut host,
            &component(&speed, &[&fingerprint(&["two"])]),
        ))?;
        let expected = Some(Selection {
            options: shown.clone(),
            ..selection("anthropic", "sonnet", None)
        });
        let mut pinned = shown;
        pinned.insert("speed".to_owned(), "two".to_owned());
        assert_eq!(
            host.requests[3],
            HostRequest::SetModel {
                scope: Scope::Conversation,
                selection: Some(Selection {
                    options: pinned,
                    ..selection("anthropic", "sonnet", None)
                }),
                expected,
            }
        );
        assert_eq!(selected(&reply, &speed), Some(fingerprint(&["two"])));
        Ok(())
    }

    #[test]
    fn a_refused_change_shows_the_current_state_without_retrying() -> Result<(), String> {
        let mut host = runtime(true);
        host.refusal = Some("The model settings changed.".to_owned());
        host.concurrent = Some(selection("anthropic", "opus", Some("high")));
        let high = fingerprint(&["high"]);
        let reply = expect_reply(interact(
            &mut host,
            &component(&thinking_control("conversation"), &[&high]),
        ))?;
        assert!(
            reply.content.contains(
                "Your change was not confirmed: The model settings changed. The settings below are current."
            ),
            "{}",
            reply.content
        );
        // The panel is rebuilt from the state read after the refusal, so it
        // shows the other writer's change and controls bound to it.
        assert!(reply.content.contains("This conversation: opus name"));
        assert!(custom_ids(&reply).contains(&option_control(
            "conversation",
            "anthropic",
            "opus",
            "thinking"
        )));
        assert_eq!(
            host.requests
                .iter()
                .filter(|request| matches!(request, HostRequest::SetModel { .. }))
                .count(),
            1,
            "a refused change is not retried"
        );
        assert_eq!(host.requests.last(), Some(&HostRequest::ModelState));

        for control in [
            component("model:default:agent", &[]),
            component("model:model:agent", &[&key("openai", "gpt")]),
        ] {
            let reply = expect_reply(interact(&mut host, &control))?;
            assert!(reply.content.contains("Your change was not confirmed"));
            assert!(
                reply
                    .content
                    .contains("Changing the default for new conversations:")
            );
            assert!(custom_ids(&reply).contains(&"model:default:agent".to_owned()));
        }
        Ok(())
    }

    #[test]
    fn a_refused_change_without_current_state_says_both() -> Result<(), String> {
        let mut host = runtime(true);
        let long = "🦀".repeat(3000);
        host.refusal = Some(long.clone());
        host.then_fail = Some(long);
        let reply = expect_reply(interact(&mut host, &component("model:default:agent", &[])))?;
        assert!(reply.content.starts_with("Your change was not confirmed: "));
        assert!(reply.content.contains("Model settings are unavailable: "));
        assert!(reply.content.ends_with("Reopen /model to refresh."));
        assert!(reply.components.is_empty());
        Ok(())
    }

    #[test]
    fn unavailable_settings_and_unknown_controls_are_reported() -> Result<(), String> {
        let mut host = runtime(true);
        host.failure = Some("runtime provider is unavailable".to_owned());
        let reply = expect_reply(interact(&mut host, &command(true)))?;
        assert_eq!(
            reply.content,
            "Model settings are unavailable: runtime provider is unavailable Reopen /model to refresh."
        );
        assert!(reply.components.is_empty());

        let mut host = runtime(true);
        for (custom_id, values) in [
            ("model:bogus:conversation", &["x"][..]),
            ("other:default:agent", &[]),
            ("model:page:conversation:many", &[]),
            ("model:model:conversation", &[]),
        ] {
            assert!(matches!(
                interact(&mut host, &component(custom_id, values)),
                Outcome::Failure(_)
            ));
        }
        Ok(())
    }
}
