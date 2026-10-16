use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{COMMAND, interact};
use crate::components::{PAGE, TEXT_LIMIT, fingerprint};
use crate::{
    HostCalls,
    contract::{
        ActivityMarkers, HostReply, HostRequest, Interaction, InteractionKind, Outcome, Reply,
    },
};
use carillon_protocol::{
    model::ModelScope as Scope,
    models::{
        AvailableModel, ModelOption, ModelProvider, ModelState, OptionChoice, ScopeState, Selection,
    },
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
                        .filter(|chosen| option.choices.iter().any(|choice| choice.id == **chosen))
                        .or(option.choices.first().map(|choice| &choice.id))
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
                Scope::Conversation => self.state.conversation.as_mut().ok_or("no conversation")?,
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
                choices: choices(thinking),
                default: thinking.first().copied().unwrap_or_default().to_owned(),
            }]
        },
    }
}

/// Choices named as a runtime names them: `high` is "High".
fn choices(ids: &[&str]) -> Vec<OptionChoice> {
    ids.iter()
        .map(|id| {
            let mut name = id.chars();
            OptionChoice {
                id: (*id).to_owned(),
                name: name.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(name).collect()
                }),
            }
        })
        .collect()
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
            providers: vec![
                ModelProvider {
                    id: "anthropic".to_owned(),
                    name: "Anthropic".to_owned(),
                },
                ModelProvider {
                    id: "openai".to_owned(),
                    name: "OpenAI".to_owned(),
                },
            ],
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
    assert!(shown(&reply).contains("This conversation\n🦀"));
    assert!(shown(&reply).contains("Default for new conversations\n🦀"));
    assert!(shown(&reply).contains('…'));
    assert!(!reply.components.is_empty());
    host.refusal = Some(long.clone());
    let reply = expect_reply(interact(
        &mut host,
        &component("model:default:conversation", &[]),
    ))?;
    assert!(shown(&reply).starts_with("⚠️ Your change was not confirmed: 🦀"));
    assert!(shown(&reply).contains("Default for new conversations\n🦀"));
    assert!(!reply.components.is_empty());
    host.failure = Some(long);
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    assert!(reply.content.starts_with("Model settings are unavailable:"));
    assert!(reply.embeds.is_empty());
    Ok(())
}

#[test]
fn a_read_only_scope_shows_its_selection_without_controls() -> Result<(), String> {
    let mut host = runtime(true);
    host.state.agent.read_only = true;
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    assert!(shown(&reply).ends_with(
        "Default for new conversations\nsonnet name · Thinking: Medium\n-# set by the runtime"
    ));
    assert!(custom_ids(&reply).contains(&"model:model:conversation".to_owned()));

    let reply = expect_reply(interact(&mut host, &component("model:scope:agent", &[])))?;
    assert!(shown(&reply).starts_with(
        "The default for new conversations is set by the runtime and cannot be changed here.\n\
         This conversation\n"
    ));
    assert_eq!(
        labels(&reply),
        ["This conversation", "Default for new conversations"]
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

#[test]
fn a_runtime_without_models_says_so_and_offers_no_controls() -> Result<(), String> {
    let mut host = runtime(true);
    host.state.available.clear();
    host.state.agent.selection = None;
    host.state.conversation = Some(host.state.agent.clone());
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    assert_eq!(
        shown(&reply),
        "No models are available from the runtime.\n\
         This conversation\nNo model selected\n-# follows the default for new conversations\n\
         Default for new conversations\nNo model selected\n-# runtime default"
    );
    assert!(reply.components.is_empty());
    Ok(())
}

fn command(conversation: bool) -> Interaction {
    Interaction {
        kind: InteractionKind::Command,
        name: COMMAND.to_owned(),
        values: Vec::new(),
        options: Value::Null,
        conversation,
        markers: ActivityMarkers::default(),
    }
}

fn component(custom_id: &str, values: &[&str]) -> Interaction {
    Interaction {
        kind: InteractionKind::Component,
        name: custom_id.to_owned(),
        values: values.iter().map(|value| (*value).to_owned()).collect(),
        options: Value::Null,
        conversation: true,
        markers: ActivityMarkers::default(),
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

/// A reply, checked against the Discord limits the module promises to
/// respect: its embeds as the host checks them, and its components.
fn expect_reply(outcome: Outcome) -> Result<Reply, String> {
    let Outcome::Reply(reply) = outcome else {
        return Err(format!("expected a reply, got {outcome:?}"));
    };
    assert!(
        reply.content.chars().count() <= 2000,
        "reply content too long"
    );
    let embeds: Vec<twilight_model::channel::message::Embed> =
        serde_json::from_value(Value::Array(reply.embeds.clone()))
            .map_err(|error| error.to_string())?;
    twilight_validate::message::embeds(&embeds).map_err(|error| error.to_string())?;
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
            assert!(
                component["placeholder"]
                    .as_str()
                    .is_some_and(|text| text.chars().count() <= 150)
            );
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

/// The reply as a person reads it: its text, then its embed's notes, each
/// section's heading and lines, and its closing hint.
fn shown(reply: &Reply) -> String {
    let mut lines = vec![reply.content.as_str()];
    for embed in &reply.embeds {
        lines.extend(embed["description"].as_str());
        for field in embed["fields"].as_array().into_iter().flatten() {
            lines.extend(field["name"].as_str());
            lines.extend(field["value"].as_str());
        }
        lines.extend(embed["footer"]["text"].as_str());
    }
    lines.retain(|line| !line.is_empty());
    lines.join("\n")
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

/// Each button's label, in order.
fn labels(reply: &Reply) -> Vec<String> {
    components(reply)
        .into_iter()
        .filter_map(|component| component["label"].as_str())
        .map(str::to_owned)
        .collect()
}

fn component_by_id<'a>(reply: &'a Reply, custom_id: &str) -> &'a Value {
    components(reply)
        .into_iter()
        .find(|component| component["custom_id"] == custom_id)
        .unwrap_or(&Value::Null)
}

fn options<'a>(reply: &'a Reply, custom_id: &str) -> &'a [Value] {
    component_by_id(reply, custom_id)["options"]
        .as_array()
        .map_or(&[], Vec::as_slice)
}

fn selected(reply: &Reply, custom_id: &str) -> Option<String> {
    options(reply, custom_id)
        .iter()
        .find(|option| option["default"] == true)?["value"]
        .as_str()
        .map(str::to_owned)
}

/// What a select shows: its chosen option's label, otherwise its placeholder.
fn showing(reply: &Reply, custom_id: &str) -> Option<String> {
    options(reply, custom_id)
        .iter()
        .find(|option| option["default"] == true)
        .map_or(
            &component_by_id(reply, custom_id)["placeholder"],
            |option| &option["label"],
        )
        .as_str()
        .map(str::to_owned)
}

fn values(reply: &Reply, custom_id: &str) -> Vec<String> {
    options(reply, custom_id)
        .iter()
        .filter_map(|option| option["value"].as_str())
        .map(str::to_owned)
        .collect()
}

fn option_labels(reply: &Reply, custom_id: &str) -> Vec<String> {
    options(reply, custom_id)
        .iter()
        .filter_map(|option| option["label"].as_str())
        .map(str::to_owned)
        .collect()
}

#[test]
fn in_a_conversation_both_scopes_show_and_the_controls_change_the_conversation()
-> Result<(), String> {
    let mut host = runtime(true);
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    assert_eq!(
        shown(&reply),
        "This conversation\nsonnet name · Thinking: Medium\n\
         -# follows the default for new conversations\n\
         Default for new conversations\nsonnet name · Thinking: Medium\n-# runtime default"
    );
    let provider = provider_control("conversation", "anthropic");
    assert_eq!(option_labels(&reply, &provider), ["Anthropic", "OpenAI"]);
    assert_eq!(showing(&reply, &provider).as_deref(), Some("Anthropic"));
    assert_eq!(
        component_by_id(&reply, &provider)["placeholder"],
        "Provider for this conversation"
    );
    let model = "model:model:conversation";
    assert_eq!(option_labels(&reply, model), ["opus name", "sonnet name"]);
    assert_eq!(showing(&reply, model).as_deref(), Some("sonnet name"));
    assert_eq!(
        options(&reply, model)
            .iter()
            .map(|option| option["description"].as_str())
            .collect::<Vec<_>>(),
        [Some("opus"), Some("sonnet")]
    );
    assert_eq!(
        component_by_id(&reply, model)["placeholder"],
        "Model for this conversation"
    );
    let thinking = thinking_control("conversation");
    assert_eq!(
        option_labels(&reply, &thinking),
        [
            "Thinking: Off",
            "Thinking: Low",
            "Thinking: Medium",
            "Thinking: High"
        ]
    );
    assert_eq!(
        showing(&reply, &thinking).as_deref(),
        Some("Thinking: Medium")
    );
    assert_eq!(
        labels(&reply),
        [
            "This conversation",
            "Default for new conversations",
            "Use default for new conversations"
        ]
    );
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
fn outside_a_conversation_only_the_default_for_new_conversations_is_offered() -> Result<(), String>
{
    let mut host = runtime(false);
    let reply = expect_reply(interact(&mut host, &command(false)))?;
    assert_eq!(
        shown(&reply),
        "Default for new conversations\nsonnet name · Thinking: Medium\n-# runtime default\n\
         Use /model inside a conversation to choose its model."
    );
    assert_eq!(
        custom_ids(&reply),
        [
            provider_control("agent", "anthropic"),
            "model:model:agent".to_owned(),
            thinking_control("agent"),
            "model:default:agent".to_owned(),
        ]
    );
    assert_eq!(
        component_by_id(&reply, "model:model:agent")["placeholder"],
        "Model for new conversations"
    );
    assert_eq!(labels(&reply), ["Use runtime default"]);
    Ok(())
}

#[test]
fn brand_emotes_come_from_the_providers_markers() -> Result<(), String> {
    let mut host = runtime(true);
    let mut interaction = command(true);
    interaction.markers.models.extend([
        ("sonnet".to_owned(), "<:claude:111>".to_owned()),
        ("opus".to_owned(), "<:claude:111>".to_owned()),
        ("gpt".to_owned(), "✳️".to_owned()),
    ]);
    let reply = expect_reply(interact(&mut host, &interaction))?;
    assert!(
        shown(&reply)
            .starts_with("This conversation\n<:claude:111> sonnet name · Thinking: Medium\n")
    );
    let claude = json!({ "id": "111", "name": "claude", "animated": false });
    let provider = provider_control("conversation", "anthropic");
    let unicode = json!({ "name": "✳️" });
    assert_eq!(
        options(&reply, &provider)
            .iter()
            .map(|option| &option["emoji"])
            .collect::<Vec<_>>(),
        [&claude, &unicode]
    );
    assert!(
        options(&reply, "model:model:conversation")
            .iter()
            .all(|option| option["emoji"] == claude)
    );

    let mut gpt = command(true);
    gpt.markers = interaction.markers.clone();
    host.state.agent.selection = Some(selection("openai", "gpt", None));
    let reply = expect_reply(interact(&mut host, &gpt))?;
    assert!(shown(&reply).contains("Default for new conversations\n✳️ gpt name\n"));
    gpt.markers.models.insert("gpt".to_owned(), "•".to_owned());
    let reply = expect_reply(interact(&mut host, &gpt))?;
    assert!(shown(&reply).contains("Default for new conversations\n• gpt name\n"));
    assert!(
        options(&reply, &provider)
            .iter()
            .find(|option| option["label"] == "OpenAI")
            .is_some_and(|option| option.get("emoji").is_none())
    );
    Ok(())
}

#[test]
fn choices_read_as_the_runtime_names_them_and_send_their_ids() -> Result<(), String> {
    let mut host = runtime(true);
    host.state.available[2].options = vec![ModelOption {
        id: "serviceTier".to_owned(),
        name: "Speed".to_owned(),
        choices: vec![
            OptionChoice {
                id: "default".to_owned(),
                name: "Standard".to_owned(),
            },
            OptionChoice {
                id: "priority".to_owned(),
                name: "Fast".to_owned(),
            },
        ],
        default: "default".to_owned(),
    }];
    expect_reply(interact(
        &mut host,
        &component("model:model:conversation", &[&key("openai", "gpt")]),
    ))?;
    let speed = option_control("conversation", "openai", "gpt", "serviceTier");
    let reply = expect_reply(interact(
        &mut host,
        &component(&speed, &[&fingerprint(&["priority"])]),
    ))?;
    assert!(shown(&reply).starts_with("This conversation\ngpt name · Speed: Fast\n"));
    assert_eq!(
        option_labels(&reply, &speed),
        ["Speed: Standard", "Speed: Fast"]
    );
    assert_eq!(showing(&reply, &speed).as_deref(), Some("Speed: Fast"));
    let mut options = BTreeMap::new();
    options.insert("serviceTier".to_owned(), "priority".to_owned());
    assert_eq!(
        host.requests[3],
        HostRequest::SetModel {
            scope: Scope::Conversation,
            selection: Some(Selection {
                options,
                ..selection("openai", "gpt", None)
            }),
            expected: Some(Selection {
                options: BTreeMap::from([("serviceTier".to_owned(), "default".to_owned())]),
                ..selection("openai", "gpt", None)
            }),
        }
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
    assert!(
        shown(&reply).starts_with("This conversation\ngpt name\n-# set for this conversation\n")
    );
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
    let thinking = option_control("conversation", "anthropic", "opus", "thinking");
    assert_eq!(showing(&reply, &thinking).as_deref(), Some("Thinking: Off"));
    let reply = expect_reply(interact(
        &mut host,
        &component(&thinking, &[&fingerprint(&["high"])]),
    ))?;
    assert_eq!(
        host.requests[5],
        HostRequest::SetModel {
            scope: Scope::Conversation,
            selection: Some(selection("anthropic", "opus", Some("high"))),
            expected: Some(selection("anthropic", "opus", Some("off"))),
        }
    );
    assert!(shown(&reply).starts_with(
        "This conversation\nopus name · Thinking: High\n-# set for this conversation\n"
    ));
    Ok(())
}

mod selection;
