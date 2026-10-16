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

mod selection;
