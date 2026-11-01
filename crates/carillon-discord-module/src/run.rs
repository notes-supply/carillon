//! `/command`: run one of the agent's own commands (or skills) in this
//! conversation. With an invocation the text is sent at once, verbatim, as
//! the person's message; bare, it lists what the runtime offers, 25 at a
//! time, and choosing one shows what it does before it runs. The runtime's
//! answer arrives in the conversation like any other.
//!
//! As in `/settings`, select values and custom IDs carry fingerprints of
//! command names, resolved against the runtime's current list on every
//! click, so a control left over from an earlier list is reported rather
//! than misapplied. Discord's interactions cannot ask for arguments after a
//! choice, so a chosen command runs without them; the panel says how to pass
//! them.

use std::fmt::Write as _;

use carillon_protocol::model::{CommandCatalog, RuntimeCommand};
use serde_json::{Value, json};

use crate::{
    HostCalls,
    components::{PAGE, PRIMARY, SECONDARY, button, choice, fingerprint, select, shorten},
    contract::{Interaction, InteractionKind, Outcome, Reply},
};

pub const COMMAND: &str = "command";
const OPTION: &str = "invocation";
/// Room for the marker the provider posts around the text.
const INVOCATION_LIMIT: usize = 1800;
const STALE: &str = "That command is no longer offered; the list below is current.";
const OUTSIDE: &str = "Use /command inside a conversation with the agent.";

pub fn definition() -> Value {
    json!({
        "type": 1,
        "name": COMMAND,
        "description": "Run one of the agent's own commands in this conversation",
        "options": [{
            "type": 3,
            "name": OPTION,
            "description": "The command and its arguments, such as: compact keep the decisions",
            "required": false,
            "max_length": INVOCATION_LIMIT,
        }],
    })
}

pub fn interact(host: &mut impl HostCalls, interaction: &Interaction) -> Outcome {
    if !interaction.conversation {
        return reply(OUTSIDE.to_owned(), Vec::new());
    }
    match interaction.kind {
        InteractionKind::Command if interaction.name == COMMAND => {
            match invocation(&interaction.options) {
                Some(text) => submit(host, &text),
                None => menu(host, 0, None),
            }
        }
        InteractionKind::Command => {
            Outcome::Failure(format!("unknown command {}", interaction.name))
        }
        InteractionKind::Component => {
            let mut parts = interaction.name.splitn(3, ':').skip(1);
            match (parts.next(), parts.next(), interaction.values.first()) {
                (Some("page"), Some(page), None) => match page.parse() {
                    Ok(page) => menu(host, page, None),
                    Err(_) => unknown(interaction),
                },
                (Some("pick"), Some(page), Some(key)) => match page.parse() {
                    Ok(page) => pick(host, page, key),
                    Err(_) => unknown(interaction),
                },
                (Some("run"), Some(key), None) => run(host, key),
                _ => unknown(interaction),
            }
        }
    }
}

fn unknown(interaction: &Interaction) -> Outcome {
    Outcome::Failure(format!("unknown control {}", interaction.name))
}

/// The invocation text, starting with `/`, when the person gave one.
fn invocation(options: &Value) -> Option<String> {
    let text = options
        .as_array()?
        .iter()
        .find(|option| option["name"] == OPTION)?["value"]
        .as_str()?
        .trim();
    if text.is_empty() {
        return None;
    }
    Some(if text.starts_with('/') {
        text.to_owned()
    } else {
        format!("/{text}")
    })
}

/// Sends the text and says what happened. A failure's reason is the
/// provider's, which says whether the text was not sent or may have been.
fn submit(host: &mut impl HostCalls, text: &str) -> Outcome {
    let shown = shorten(text, 200).replace('`', "ˋ");
    match host.submit_command(text.to_owned()) {
        Ok(()) => reply(
            format!("Sent `{shown}` to the agent; its answer follows in this conversation."),
            Vec::new(),
        ),
        Err(reason) => reply(format!("`{shown}`: {}", shorten(&reason, 400)), Vec::new()),
    }
}

/// One page of the runtime's commands; `note` explains why it is shown.
fn menu(host: &mut impl HostCalls, page: usize, note: Option<&str>) -> Outcome {
    let catalog = match host.commands() {
        Ok(catalog) => catalog,
        Err(error) => return unavailable(&error),
    };
    let mut content = String::new();
    if let Some(note) = note {
        let _ = writeln!(content, "{note}");
    }
    if catalog.commands.is_empty() {
        content.push_str(
            "The agent's runtime lists no commands here. You can still run one by name with `/command invocation:`.",
        );
        return reply(content, Vec::new());
    }
    let pages = catalog.commands.len().div_ceil(PAGE);
    let page = page.min(pages - 1);
    let _ = write!(
        content,
        "**Commands** — {} available. Choose one to see what it does before it runs; to pass arguments, use `/command invocation:`.",
        catalog.commands.len()
    );
    let first = page * PAGE;
    let shown = &catalog.commands[first..(first + PAGE).min(catalog.commands.len())];
    let mut rows = vec![select(
        &format!("{COMMAND}:pick:{page}"),
        &if pages > 1 {
            format!(
                "Command — showing {}–{} of {}",
                first + 1,
                first + shown.len(),
                catalog.commands.len()
            )
        } else {
            "Command".to_owned()
        },
        shown.iter().map(|command| {
            choice(
                &format!("/{}", command.name),
                &fingerprint(&[&command.name]),
                command.description.as_deref().or(command.hint.as_deref()),
                false,
            )
        }),
    )];
    if pages > 1 {
        rows.push(json!({ "type": 1, "components": [
            button(&format!("{COMMAND}:page:{}", page.saturating_sub(1)), "◀ Previous", SECONDARY, page == 0),
            button(&format!("{COMMAND}:page:{}", page + 1), "Next ▶", SECONDARY, page + 1 == pages),
        ]}));
    }
    reply(content, rows)
}

/// What the chosen command does, with a button that runs it.
fn pick(host: &mut impl HostCalls, page: usize, key: &str) -> Outcome {
    let catalog = match host.commands() {
        Ok(catalog) => catalog,
        Err(error) => return unavailable(&error),
    };
    let Some(command) = find(&catalog, key) else {
        return menu(host, page, Some(STALE));
    };
    let mut content = format!("**/{}**", shorten(&command.name, 100));
    if let Some(description) = &command.description {
        let _ = write!(content, " — {}", shorten(description, 1000));
    }
    if let Some(hint) = &command.hint {
        let _ = write!(
            content,
            "\nArguments: {}\nRunning it here sends it without arguments; to pass them, use `/command invocation:{} …`.",
            shorten(hint, 300),
            shorten(&command.name, 100)
        );
    }
    let buttons = [
        button(&format!("{COMMAND}:run:{key}"), "Run", PRIMARY, false),
        button(&format!("{COMMAND}:page:{page}"), "Back", SECONDARY, false),
    ];
    reply(content, vec![json!({ "type": 1, "components": buttons })])
}

fn run(host: &mut impl HostCalls, key: &str) -> Outcome {
    let catalog = match host.commands() {
        Ok(catalog) => catalog,
        Err(error) => return unavailable(&error),
    };
    match find(&catalog, key) {
        Some(command) => submit(host, &format!("/{}", command.name)),
        None => menu(host, 0, Some(STALE)),
    }
}

fn find<'a>(catalog: &'a CommandCatalog, key: &str) -> Option<&'a RuntimeCommand> {
    catalog
        .commands
        .iter()
        .find(|command| fingerprint(&[&command.name]) == key)
}

fn unavailable(error: &str) -> Outcome {
    reply(
        format!("Commands are unavailable: {}", shorten(error, 1700)),
        Vec::new(),
    )
}

fn reply(content: String, components: Vec<Value>) -> Outcome {
    Outcome::Reply(Reply {
        content,
        embeds: Vec::new(),
        components,
    })
}

#[cfg(test)]
mod tests {
    use carillon_protocol::model::{CommandCatalog, RuntimeCommand};
    use serde_json::{Value, json};

    use super::{COMMAND, interact};
    use crate::{
        HostCalls,
        components::fingerprint,
        contract::{
            ActivityMarkers, HostReply, HostRequest, Intent, Interaction, InteractionKind, Outcome,
            Reply,
        },
    };

    /// Lists a fixed catalogue and records submissions.
    struct FakeHost {
        catalog: CommandCatalog,
        submitted: Vec<String>,
        refusal: Option<String>,
    }

    impl HostCalls for FakeHost {
        fn call(&mut self, request: HostRequest) -> Result<HostReply, String> {
            match request {
                HostRequest::Commands => Ok(HostReply::Commands(self.catalog.clone())),
                HostRequest::Submit {
                    text,
                    intent: Intent::Command,
                } => {
                    if let Some(refusal) = &self.refusal {
                        return Err(refusal.clone());
                    }
                    self.submitted.push(text);
                    Ok(HostReply::Submitted)
                }
                other => Err(format!("unexpected {other:?}")),
            }
        }
    }

    fn host(count: usize) -> FakeHost {
        FakeHost {
            catalog: CommandCatalog {
                commands: (0..count)
                    .map(|index| RuntimeCommand {
                        name: if index == 0 {
                            "compact".to_owned()
                        } else {
                            format!("command-{index}")
                        },
                        description: (index == 0).then(|| "Summarize the conversation".to_owned()),
                        hint: (index == 0).then(|| "<instructions>".to_owned()),
                    })
                    .collect(),
            },
            submitted: Vec::new(),
            refusal: None,
        }
    }

    fn command(invocation: Option<&str>, conversation: bool) -> Interaction {
        Interaction {
            kind: InteractionKind::Command,
            name: COMMAND.to_owned(),
            values: Vec::new(),
            options: invocation.map_or(
                Value::Null,
                |text| json!([{ "name": "invocation", "type": 3, "value": text }]),
            ),
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

    fn expect_reply(outcome: Outcome) -> Result<Reply, String> {
        match outcome {
            Outcome::Reply(reply) => {
                assert!(reply.content.chars().count() <= 2000);
                assert!(reply.components.len() <= 5);
                Ok(reply)
            }
            other => Err(format!("expected a reply, got {other:?}")),
        }
    }

    fn options(reply: &Reply) -> Vec<String> {
        reply.components[0]["components"][0]["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| option["label"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn an_invocation_is_sent_verbatim_as_a_command() -> Result<(), String> {
        let mut host = host(3);
        let reply = expect_reply(interact(
            &mut host,
            &command(Some("  compact keep the `decisions`  "), true),
        ))?;
        assert_eq!(host.submitted, ["/compact keep the `decisions`"]);
        assert_eq!(
            reply.content,
            "Sent `/compact keep the ˋdecisionsˋ` to the agent; its answer follows in this conversation."
        );
        expect_reply(interact(&mut host, &command(Some("/review main"), true)))?;
        assert_eq!(host.submitted[1], "/review main");

        host.refusal = Some("Carillon could not accept the command right now.".to_owned());
        let reply = expect_reply(interact(&mut host, &command(Some("status"), true)))?;
        assert_eq!(
            reply.content,
            "`/status`: Carillon could not accept the command right now."
        );
        // An unconfirmed submission may still run: the reply says so, and
        // never that it was not sent.
        let uncertain = "Carillon did not confirm that it accepted it; if it did, the answer follows in this conversation.";
        host.refusal = Some(uncertain.to_owned());
        let reply = expect_reply(interact(&mut host, &command(Some("status"), true)))?;
        assert_eq!(reply.content, format!("`/status`: {uncertain}"));
        assert_eq!(host.submitted.len(), 2);

        let reply = expect_reply(interact(&mut host, &command(Some("status"), false)))?;
        assert_eq!(
            reply.content,
            "Use /command inside a conversation with the agent."
        );
        assert_eq!(host.submitted.len(), 2);
        Ok(())
    }

    #[test]
    fn every_command_is_reachable_through_pages_and_runs_after_a_confirmation() -> Result<(), String>
    {
        let mut host = host(64);
        let first = expect_reply(interact(&mut host, &command(None, true)))?;
        assert!(first.content.contains("64 available"));
        assert_eq!(options(&first).len(), 25);
        assert_eq!(options(&first)[0], "/compact");
        let mut seen = options(&first);
        for page in 1..3 {
            let reply = expect_reply(interact(
                &mut host,
                &component(&format!("command:page:{page}"), &[]),
            ))?;
            seen.extend(options(&reply));
        }
        assert_eq!(seen.len(), 64);
        assert_eq!(seen[63], "/command-63");

        // Choosing shows the command first; nothing runs until Run.
        let key = fingerprint(&["compact"]);
        let chosen = expect_reply(interact(&mut host, &component("command:pick:0", &[&key])))?;
        assert!(
            chosen
                .content
                .starts_with("**/compact** — Summarize the conversation")
        );
        assert!(chosen.content.contains("Arguments: <instructions>"));
        assert!(host.submitted.is_empty());
        let run = format!("command:run:{key}");
        let ids: Vec<&str> = chosen.components[0]["components"]
            .as_array()
            .map(|buttons| {
                buttons
                    .iter()
                    .filter_map(|b| b["custom_id"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(ids, [run.as_str(), "command:page:0"]);
        let sent = expect_reply(interact(&mut host, &component(&run, &[])))?;
        assert_eq!(host.submitted, ["/compact"]);
        assert!(sent.content.starts_with("Sent `/compact`"));

        // A control from an earlier list is reported, not misapplied.
        host.catalog.commands.remove(0);
        let stale = expect_reply(interact(&mut host, &component(&run, &[])))?;
        assert!(
            stale
                .content
                .starts_with("That command is no longer offered")
        );
        assert_eq!(host.submitted.len(), 1);
        Ok(())
    }

    #[test]
    fn an_empty_list_still_explains_how_to_run_a_command() -> Result<(), String> {
        let mut host = host(0);
        let reply = expect_reply(interact(&mut host, &command(None, true)))?;
        assert!(reply.components.is_empty());
        assert!(reply.content.contains("`/command invocation:`"));
        Ok(())
    }
}
