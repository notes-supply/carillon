use anyhow::Result;
use carillon_discord_module::contract::Reply;
use serde_json::{Value, json};

use super::answer;

fn reply(content: &str, embeds: Vec<Value>, components: Vec<Value>) -> Reply {
    Reply {
        content: content.to_owned(),
        embeds,
        components,
    }
}

fn refusal(reply: &Reply) -> String {
    answer(reply).map_or_else(|error| error.to_string(), |_| "accepted".to_owned())
}

#[test]
fn oversized_reply_content_is_rejected_before_the_discord_edit() -> Result<()> {
    let mut long = reply(&"🦀".repeat(2000), Vec::new(), Vec::new());
    answer(&long)?;
    long.content.push('x');
    assert_eq!(refusal(&long), "Discord module reply has unusable content");
    Ok(())
}

#[test]
fn native_selects_need_no_string_options() -> Result<()> {
    for kind in [5, 6, 7, 8] {
        let native = json!({
            "type": 1,
            "components": [{"type": kind, "custom_id": "native", "min_values": 1, "max_values": 1}]
        });
        let shown = answer(&reply("Choose", Vec::new(), vec![native]))?;
        assert_eq!(shown.components.len(), 1, "type {kind}");
    }
    for (options, refused) in [
        (Value::Null, "Discord module reply has invalid components"),
        (
            json!([]),
            "Discord module reply has a select menu without options",
        ),
    ] {
        let string = json!({
            "type": 1, "components": [{"type": 3, "custom_id": "string", "options": options}]
        });
        assert_eq!(
            refusal(&reply("Choose", Vec::new(), vec![string])),
            refused,
            "options {options}"
        );
    }
    Ok(())
}

#[test]
fn reply_embeds_are_held_to_the_limits_a_card_is() -> Result<()> {
    let embed = |description: &str| json!({"type": "rich", "description": description});
    let shown = answer(&reply("", vec![embed("Model settings")], Vec::new()))?;
    assert_eq!(shown.embeds.len(), 1);
    assert_eq!(
        shown.embeds[0].description.as_deref(),
        Some("Model settings")
    );
    for (case, embeds) in [
        ("not an embed", vec![json!({"description": "no type"})]),
        (
            "a description over its limit",
            vec![embed(&"x".repeat(4_097))],
        ),
        (
            "embeds over their combined limit",
            vec![embed(&"x".repeat(3_500)), embed(&"x".repeat(3_500))],
        ),
        ("more embeds than a message holds", vec![embed("one"); 11]),
    ] {
        assert_eq!(
            refusal(&reply("", embeds, Vec::new())),
            "Discord module reply has unusable embeds",
            "{case}"
        );
    }
    Ok(())
}
