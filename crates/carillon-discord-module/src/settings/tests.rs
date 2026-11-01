use std::collections::BTreeMap;

use carillon_protocol::settings::{
    Change, Choice, Page, Revision, RuntimePart, RuntimeSettings, Setting, Subject,
    Value as SettingValue,
};
use serde_json::Value;

use super::{COMMAND, interact};
use crate::{
    HostCalls,
    components::{NEXT, PAGE, PREVIOUS},
    contract::{
        ActivityMarkers, HostReply, HostRequest, Interaction, InteractionKind, Notice, Outcome,
        Reply,
    },
};

struct FakeHost {
    page: Page,
    answer: Option<Notice>,
    requests: Vec<HostRequest>,
    revisions: u64,
}

impl FakeHost {
    fn new(page: Page) -> Self {
        Self {
            page,
            answer: None,
            requests: Vec::new(),
            revisions: 1000,
        }
    }

    fn changes(&self) -> Vec<&Change> {
        self.requests
            .iter()
            .filter_map(|request| match request {
                HostRequest::ChangeSetting(change) => Some(change),
                _ => None,
            })
            .collect()
    }

    fn changed_elsewhere(&mut self, setting: &str, id: &str) -> Result<(), String> {
        self.revisions += 1;
        let revision = revision(self.revisions)?;
        let line = line_mut(&mut self.page, setting)?;
        line.value = Some(one(id));
        line.inherited = false;
        line.revision = revision;
        Ok(())
    }
}

impl HostCalls for FakeHost {
    fn call(&mut self, request: HostRequest) -> Result<HostReply, String> {
        self.requests.push(request.clone());
        let notice = match request {
            HostRequest::Settings => None,
            HostRequest::ChangeSetting(_) if self.answer.is_some() => self.answer.clone(),
            HostRequest::ChangeSetting(change) => {
                self.revisions += 1;
                let revision = revision(self.revisions)?;
                let line = line_mut(&mut self.page, &change.setting)?;
                if line.revision != change.expected {
                    return Err("the fake owner was sent a stale change".to_owned());
                }
                line.inherited = change.value.is_none();
                line.value = Some(change.value.unwrap_or_else(|| one("medium")));
                line.revision = revision;
                None
            }
            _ => return Err("not a settings call".to_owned()),
        };
        Ok(HostReply::Page {
            page: self.page.clone(),
            notice,
        })
    }
}

fn line_mut<'a>(page: &'a mut Page, setting: &str) -> Result<&'a mut Setting, String> {
    let RuntimePart::Settings(runtime) = &mut page.runtime else {
        return Err("the runtime is unavailable".to_owned());
    };
    runtime
        .settings
        .iter_mut()
        .find(|line| line.id == setting)
        .ok_or_else(|| format!("no line {setting}"))
}

fn revision(seed: u64) -> Result<Revision, String> {
    Revision::try_from(format!("{seed:064x}"))
}

fn one(id: &str) -> SettingValue {
    SettingValue::One { id: id.to_owned() }
}

fn named(id: &str, name: &str) -> Choice {
    Choice {
        id: id.to_owned(),
        name: name.to_owned(),
        group: None,
    }
}

fn line(
    id: &str,
    name: &str,
    choices: Vec<Choice>,
    value: Option<&str>,
    inherited: bool,
    seed: u64,
) -> Result<Setting, String> {
    Ok(Setting {
        id: id.to_owned(),
        name: name.to_owned(),
        choices,
        value: value.map(one),
        inherited,
        revision: revision(seed)?,
    })
}

fn route() -> Result<Vec<Setting>, String> {
    Ok(vec![
        line(
            "agent",
            "Agent",
            vec![named("assistant", "assistant")],
            Some("assistant"),
            false,
            1,
        )?,
        line(
            "listens",
            "Listens",
            vec![named("always", "always")],
            Some("always"),
            false,
            2,
        )?,
        line(
            "replies",
            "Replies",
            vec![named("thread", "threads")],
            Some("thread"),
            false,
            3,
        )?,
    ])
}

fn page(runtime: RuntimePart) -> Result<Page, String> {
    Ok(Page {
        title: "<#300>".to_owned(),
        subject: Subject::Channel {
            interface_id: "discord".to_owned(),
            channel_id: "300".to_owned(),
            agent_id: "assistant".to_owned(),
        },
        interface: route()?,
        runtime,
    })
}

fn models() -> Vec<Choice> {
    let model = |id: &str, name: &str, group: &str| Choice {
        id: id.to_owned(),
        name: name.to_owned(),
        group: Some(group.to_owned()),
    };
    vec![
        model("openai-codex/gpt-6.1-sol", "GPT-6.1 Sol", "OpenAI Codex"),
        model("openai-codex/gpt-6.1-mini", "GPT-6.1 Mini", "OpenAI Codex"),
        model("local/echo", "Echo", "Local"),
    ]
}

fn thinking() -> Vec<Choice> {
    vec![
        named("low", "Low"),
        named("medium", "Medium"),
        named("high", "High"),
    ]
}

fn home() -> Result<Page, String> {
    page(RuntimePart::Settings(RuntimeSettings {
        settings: vec![
            line(
                "model",
                "Model",
                models(),
                Some("openai-codex/gpt-6.1-sol"),
                true,
                10,
            )?,
            line(
                "model.thinking",
                "Thinking",
                thinking(),
                Some("medium"),
                true,
                11,
            )?,
        ],
    }))
}

fn interaction(kind: InteractionKind, name: &str, values: &[&str]) -> Interaction {
    Interaction {
        kind,
        name: name.to_owned(),
        values: values.iter().map(|value| (*value).to_owned()).collect(),
        options: Value::Null,
        conversation: false,
        markers: ActivityMarkers::default(),
    }
}

fn command() -> Interaction {
    interaction(InteractionKind::Command, COMMAND, &[])
}

fn click(custom_id: &str, values: &[&str]) -> Interaction {
    interaction(InteractionKind::Component, custom_id, values)
}

fn expect_reply(outcome: Outcome) -> Result<Reply, String> {
    let Outcome::Reply(reply) = outcome else {
        return Err(format!("expected a reply, got {outcome:?}"));
    };
    twilight_validate::message::content(&reply.content).map_err(|error| error.to_string())?;
    assert!(reply.embeds.is_empty());
    assert!(reply.components.len() <= 5, "too many rows");
    let rows: Vec<twilight_model::channel::message::Component> =
        serde_json::from_value(Value::Array(reply.components.clone()))
            .map_err(|error| error.to_string())?;
    for row in &rows {
        twilight_validate::component::component_v1(row).map_err(|error| error.to_string())?;
    }
    for id in custom_ids(&reply) {
        assert!(id.len() <= 100, "{id} is longer than Discord takes");
    }
    for options in reply
        .components
        .iter()
        .filter_map(|row| row["components"][0]["options"].as_array())
    {
        assert!(!options.is_empty() && options.len() <= PAGE);
    }
    Ok(reply)
}

fn run(host: &mut FakeHost, interaction: &Interaction) -> Result<Reply, String> {
    expect_reply(interact(host, interaction))
}

fn components(reply: &Reply) -> impl Iterator<Item = &Value> {
    reply
        .components
        .iter()
        .filter_map(|row| row["components"].as_array())
        .flatten()
}

fn custom_ids(reply: &Reply) -> Vec<String> {
    components(reply)
        .filter_map(|component| component["custom_id"].as_str())
        .map(str::to_owned)
        .collect()
}

fn control(reply: &Reply, control: &str) -> Result<String, String> {
    let prefix = format!("{COMMAND}:{control}:");
    custom_ids(reply)
        .into_iter()
        .find(|id| id.starts_with(&prefix))
        .ok_or_else(|| format!("no {control} control in {:#?}", reply.components))
}

fn offers(reply: &Reply, control: &str) -> bool {
    let prefix = format!("{COMMAND}:{control}:");
    custom_ids(reply).iter().any(|id| id.starts_with(&prefix))
}

fn options<'a>(reply: &'a Reply, custom_id: &str) -> Vec<&'a Value> {
    components(reply)
        .filter(|component| component["custom_id"] == custom_id)
        .filter_map(|component| component["options"].as_array())
        .flatten()
        .collect()
}

fn labels(reply: &Reply, custom_id: &str) -> Vec<String> {
    options(reply, custom_id)
        .into_iter()
        .filter_map(|option| option["label"].as_str())
        .map(str::to_owned)
        .collect()
}

fn value(reply: &Reply, custom_id: &str, label: &str) -> Result<String, String> {
    options(reply, custom_id)
        .into_iter()
        .find(|option| option["label"] == label)
        .and_then(|option| option["value"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("no option {label} in {custom_id}"))
}

fn selected(reply: &Reply, custom_id: &str) -> Vec<String> {
    options(reply, custom_id)
        .into_iter()
        .filter(|option| option["default"] == true)
        .filter_map(|option| option["label"].as_str())
        .map(str::to_owned)
        .collect()
}

fn pick(host: &mut FakeHost, reply: &Reply, label: &str) -> Result<Reply, String> {
    let picker = control(reply, "pick")?;
    let line = value(reply, &picker, label)?;
    run(host, &click(&picker, &[&line]))
}

fn choose(host: &mut FakeHost, reply: &Reply, label: &str) -> Result<Reply, String> {
    let picker = control(reply, "set")?;
    let choice = value(reply, &picker, label)?;
    run(host, &click(&picker, &[&choice]))
}

#[test]
fn the_page_reads_as_the_contract_draws_it() -> Result<(), String> {
    let mut host = FakeHost::new(home()?);
    let reply = run(&mut host, &command())?;
    assert_eq!(
        reply.content,
        "<#300>\n```\n\
         Agent     assistant\n\
         Listens   always\n\
         Replies   threads\n\
         Model     GPT-6.1 Sol · default\n\
         Thinking  Medium · default\n\
         ```"
    );
    let lines = control(&reply, "pick")?;
    assert_eq!(
        labels(&reply, &lines),
        ["Model", "Thinking"],
        "route lines offer nothing besides their value"
    );
    assert_eq!(selected(&reply, &lines), ["Model"]);
    let models = control(&reply, "set")?;
    assert_eq!(
        labels(&reply, &models),
        ["GPT-6.1 Sol", "GPT-6.1 Mini", "Echo"]
    );
    assert_eq!(selected(&reply, &models), ["GPT-6.1 Sol"]);
    let groups: Vec<_> = options(&reply, &models)
        .into_iter()
        .map(|option| option["description"].clone())
        .collect();
    assert_eq!(groups, ["OpenAI Codex", "OpenAI Codex", "Local"]);
    assert!(!offers(&reply, "reset"), "nothing is set on the channel");

    host.changed_elsewhere("model.thinking", "high")?;
    let reply = run(&mut host, &command())?;
    assert!(
        reply.content.contains("\nThinking  High\n"),
        "{}",
        reply.content
    );
    assert!(!offers(&reply, "reset"), "Model is picked");
    let reply = pick(&mut host, &reply, "Thinking")?;
    assert_eq!(selected(&reply, &control(&reply, "set")?), ["High"]);
    assert!(offers(&reply, "reset"));
    assert!(host.changes().is_empty(), "drawing changes nothing");
    Ok(())
}

#[test]
fn an_agent_without_a_model_shows_an_empty_model_line() -> Result<(), String> {
    let mut host = FakeHost::new(page(RuntimePart::Settings(RuntimeSettings {
        settings: vec![line("model", "Model", models(), None, true, 10)?],
    }))?);
    let reply = run(&mut host, &command())?;
    assert_eq!(
        reply.content,
        "<#300>\n```\nAgent    assistant\nListens  always\nReplies  threads\nModel\n```"
    );
    let models = control(&reply, "set")?;
    assert!(selected(&reply, &models).is_empty());
    assert!(!offers(&reply, "reset"));
    Ok(())
}

#[test]
fn a_runtime_line_pinned_without_alternatives_offers_reset_and_route_lines_never_do()
-> Result<(), String> {
    for choices in [vec![named("local/echo", "Echo")], Vec::new()] {
        let offered = choices.len();
        let mut host = FakeHost::new(page(RuntimePart::Settings(RuntimeSettings {
            settings: vec![line(
                "model",
                "Model",
                choices,
                Some("local/echo"),
                false,
                10,
            )?],
        }))?);
        let reply = run(&mut host, &command())?;
        assert_eq!(
            labels(&reply, &control(&reply, "pick")?),
            ["Model"],
            "route lines are set on the channel too, and have nothing to return to"
        );
        assert_eq!(offers(&reply, "set"), offered > 0);
        run(&mut host, &click(&control(&reply, "reset")?, &[]))?;
        assert_eq!(
            host.changes(),
            [&Change {
                setting: "model".to_owned(),
                value: None,
                expected: revision(10)?,
            }]
        );
    }
    Ok(())
}

fn catalogue() -> Result<Page, String> {
    let models = (0..30)
        .map(|index| Choice {
            id: format!("provider{}/model-{index}", index % 2),
            name: format!("Model {index}"),
            group: Some(format!("Provider {}", index % 2)),
        })
        .collect();
    let mut settings = vec![line(
        "model",
        "Model",
        models,
        Some("provider1/model-29"),
        false,
        10,
    )?];
    for index in 0..25 {
        settings.push(line(
            &format!("model.flag{index}"),
            &format!("Flag {index}"),
            vec![named("off", "Off"), named("on", "On")],
            Some("off"),
            true,
            11 + index,
        )?);
    }
    page(RuntimePart::Settings(RuntimeSettings { settings }))
}

fn reachable(host: &mut FakeHost, mut reply: Reply, picker: &str) -> Result<Vec<String>, String> {
    let mut seen = Vec::new();
    for _ in 0..10 {
        let id = control(&reply, picker)?;
        let shown = labels(&reply, &id);
        let more = shown.iter().any(|label| label == "Next ▶");
        seen.extend(
            shown
                .into_iter()
                .filter(|label| label != "◀ Previous" && label != "Next ▶"),
        );
        if !more {
            return Ok(seen);
        }
        assert_eq!(value(&reply, &id, "Next ▶")?, NEXT);
        reply = run(host, &click(&id, &[NEXT]))?;
    }
    Err("the pages never ended".to_owned())
}

#[test]
fn both_pickers_page_a_26_line_catalogue_with_every_line_reachable() -> Result<(), String> {
    let mut host = FakeHost::new(catalogue()?);
    let reply = run(&mut host, &command())?;
    assert_eq!(reply.content.lines().count(), 3 + 29);
    let mut lines = vec!["Model".to_owned()];
    lines.extend((0..25).map(|index| format!("Flag {index}")));
    assert_eq!(reachable(&mut host, reply.clone(), "pick")?, lines);

    let models = control(&reply, "set")?;
    assert!(
        models.ends_with(":1"),
        "the page holding the 30th model opens"
    );
    assert_eq!(selected(&reply, &models), ["Model 29"]);
    let first = run(&mut host, &click(&models, &[PREVIOUS]))?;
    let expected: Vec<String> = (0..30).map(|index| format!("Model {index}")).collect();
    assert_eq!(reachable(&mut host, first, "set")?, expected);

    let second = run(&mut host, &click(&control(&reply, "pick")?, &[NEXT]))?;
    let flag = pick(&mut host, &second, "Flag 24")?;
    assert!(
        control(&flag, "pick")?.ends_with(":1"),
        "a line picked on the second page keeps it"
    );
    assert_eq!(labels(&flag, &control(&flag, "set")?), ["Off", "On"]);
    assert!(host.changes().is_empty(), "turning pages changes nothing");
    assert!(
        host.requests
            .iter()
            .all(|request| *request == HostRequest::Settings),
        "every click reads the page afresh"
    );
    Ok(())
}

#[test]
fn custom_ids_stay_within_discords_limit() -> Result<(), String> {
    let mut host = FakeHost::new(catalogue()?);
    let reply = run(&mut host, &command())?;
    let longest = custom_ids(&reply)
        .into_iter()
        .map(|id| id.len())
        .max()
        .unwrap_or_default();
    assert_eq!(
        longest,
        "settings:set:".len() + 16 + 1 + 64 + 1 + 1,
        "a fingerprint, the revision and a page"
    );
    let reset = control(&reply, "reset")?;
    assert_eq!(reset.len(), 15 + 16 + 1 + 64);
    Ok(())
}

#[test]
fn a_click_applies_at_once_with_the_revision_it_was_drawn_with() -> Result<(), String> {
    let mut host = FakeHost::new(home()?);
    let reply = run(&mut host, &command())?;
    let reply = pick(&mut host, &reply, "Thinking")?;
    let drawn = control(&reply, "set")?;
    let reply = choose(&mut host, &reply, "High")?;
    assert_eq!(
        host.changes(),
        [&Change {
            setting: "model.thinking".to_owned(),
            value: Some(one("high")),
            expected: revision(11)?,
        }]
    );
    assert!(drawn.contains(&revision(11)?.to_string()));
    assert!(
        reply.content.contains("\nThinking  High\n"),
        "{}",
        reply.content
    );
    assert!(!reply.content.starts_with('⚠'));
    assert_eq!(selected(&reply, &control(&reply, "pick")?), ["Thinking"]);
    let reply = run(&mut host, &click(&control(&reply, "reset")?, &[]))?;
    assert_eq!(host.changes().len(), 2);
    assert_eq!(host.changes()[1].value, None);
    assert!(
        reply.content.contains("\nThinking  Medium · default\n"),
        "{}",
        reply.content
    );
    assert!(!offers(&reply, "reset"));
    Ok(())
}

#[test]
fn a_stale_click_redraws_as_changed_meanwhile_and_sends_nothing() -> Result<(), String> {
    let mut host = FakeHost::new(home()?);
    let reply = run(&mut host, &command())?;
    let reply = pick(&mut host, &reply, "Thinking")?;
    host.changed_elsewhere("model.thinking", "low")?;
    let redrawn = choose(&mut host, &reply, "High")?;
    assert!(host.changes().is_empty(), "a stale click sent a change");
    assert!(
        redrawn
            .content
            .starts_with("⚠ Changed meanwhile\n<#300>\n```\n"),
        "{}",
        redrawn.content
    );
    assert!(redrawn.content.contains("\nThinking  Low\n"));
    assert_eq!(selected(&redrawn, &control(&redrawn, "set")?), ["Low"]);
    let reset = run(&mut host, &click(&control(&redrawn, "reset")?, &[]))?;
    assert_eq!(host.changes().len(), 1, "the redrawn card applies");
    assert!(!reset.content.starts_with('⚠'));
    Ok(())
}

#[test]
fn an_unconfirmed_change_says_so_and_is_not_retried() -> Result<(), String> {
    let mut host = FakeHost::new(home()?);
    host.answer = Some(Notice::Unconfirmed);
    let reply = run(&mut host, &command())?;
    let reply = choose(&mut host, &reply, "GPT-6.1 Mini")?;
    assert_eq!(host.changes().len(), 1);
    assert!(
        reply.content.starts_with("⚠ Not confirmed\n<#300>\n"),
        "{}",
        reply.content
    );
    host.answer = Some(Notice::Refused("Echo is not offered here.".to_owned()));
    let reply = choose(&mut host, &reply, "Echo")?;
    assert_eq!(host.changes().len(), 2);
    assert!(
        reply
            .content
            .starts_with("⚠ Echo is not offered here.\n<#300>\n"),
        "{}",
        reply.content
    );
    Ok(())
}

#[test]
fn route_lines_draw_while_the_runtime_is_unavailable() -> Result<(), String> {
    let mut host = FakeHost::new(page(RuntimePart::Unavailable {
        reason: "The runtime is not reachable right now.".to_owned(),
    })?);
    let reply = run(&mut host, &command())?;
    assert_eq!(
        reply.content,
        "⚠ The runtime is not reachable right now.\n<#300>\n```\n\
         Agent    assistant\n\
         Listens  always\n\
         Replies  threads\n\
         ```"
    );
    assert!(reply.components.is_empty(), "no line is editable");
    Ok(())
}

#[test]
fn a_thread_draws_its_channels_page() -> Result<(), String> {
    let mut host = FakeHost::new(home()?);
    let channel = run(&mut host, &command())?;
    let thread = run(
        &mut host,
        &Interaction {
            conversation: true,
            ..command()
        },
    )?;
    assert_eq!(thread, channel);
    Ok(())
}

#[test]
fn model_choices_wear_the_operators_badges_and_the_page_none() -> Result<(), String> {
    let mut host = FakeHost::new(home()?);
    let badged = |interaction: Interaction| Interaction {
        markers: ActivityMarkers {
            models: BTreeMap::from([(
                "gpt-".to_owned(),
                "<:openai:1300000000000000002>".to_owned(),
            )]),
            ..ActivityMarkers::default()
        },
        ..interaction
    };
    let reply = run(&mut host, &badged(command()))?;
    let emoji: Vec<_> = options(&reply, &control(&reply, "set")?)
        .into_iter()
        .map(|option| option["emoji"].clone())
        .collect();
    let openai =
        serde_json::json!({"id": "1300000000000000002", "name": "openai", "animated": false});
    assert_eq!(emoji, [openai.clone(), openai, Value::Null]);
    assert!(!reply.content.contains("openai"), "{}", reply.content);
    let lines = control(&reply, "pick")?;
    let thinking = run(
        &mut host,
        &badged(click(&lines, &[&value(&reply, &lines, "Thinking")?])),
    )?;
    assert!(
        components(&thinking)
            .filter_map(|component| component["options"].as_array())
            .flatten()
            .all(|option| option.get("emoji").is_none()),
        "only model choices wear badges"
    );
    Ok(())
}

#[test]
fn a_page_beyond_discords_limit_is_cut_where_it_ends() -> Result<(), String> {
    let mut settings = Vec::new();
    for index in 0..60 {
        settings.push(line(
            &format!("model.option{index}"),
            &format!("A long option name {index}"),
            vec![named("value", &"a long value name ".repeat(2))],
            Some("value"),
            true,
            100 + index,
        )?);
    }
    let mut host = FakeHost::new(page(RuntimePart::Settings(RuntimeSettings { settings }))?);
    let reply = run(&mut host, &command())?;
    assert!(reply.content.ends_with("\n…\n```"), "{}", reply.content);
    assert!(reply.content.chars().count() <= 2000);
    Ok(())
}

#[test]
fn a_click_while_the_runtime_is_out_shows_only_its_reason() -> Result<(), String> {
    let mut host = FakeHost::new(home()?);
    let reply = run(&mut host, &command())?;
    let reply = pick(&mut host, &reply, "Thinking")?;
    host.page.runtime = RuntimePart::Unavailable {
        reason: "The runtime did not answer in time.".to_owned(),
    };
    let redrawn = choose(&mut host, &reply, "High")?;
    assert!(
        redrawn
            .content
            .starts_with("⚠ The runtime did not answer in time.\n<#300>\n"),
        "{}",
        redrawn.content
    );
    assert!(host.changes().is_empty(), "nothing is sent");
    Ok(())
}
