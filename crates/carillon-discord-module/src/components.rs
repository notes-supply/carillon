//! Discord component building blocks shared by the module's commands.

use serde_json::{Value, json};

/// Discord shows at most this many options per select menu.
pub const PAGE: usize = 25;
/// Discord's length limit for select labels, values, and descriptions.
pub const TEXT_LIMIT: usize = 100;
/// Discord's length limit for a select's placeholder.
const PLACEHOLDER_LIMIT: usize = 150;
pub const PRIMARY: u8 = 1;
pub const SECONDARY: u8 = 2;

/// FNV-1a over the length-prefixed parts, as 16 hex digits.
pub fn fingerprint(parts: &[&str]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.len().to_le_bytes().into_iter().chain(part.bytes()) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

pub fn select(custom_id: &str, placeholder: &str, options: impl Iterator<Item = Value>) -> Value {
    json!({
        "type": 1,
        "components": [{
            "type": 3,
            "custom_id": custom_id,
            "placeholder": shorten(placeholder, PLACEHOLDER_LIMIT),
            "options": options.take(PAGE).collect::<Vec<_>>(),
        }],
    })
}

pub fn choice(label: &str, value: &str, description: Option<&str>, default: bool) -> Value {
    let mut option = json!({ "label": text(label), "value": value, "default": default });
    if let Some(description) = description {
        option["description"] = text(description).into();
    }
    option
}

/// The option with the marker as its emoji. Only a custom emote becomes one:
/// Discord refuses a symbol that is not an emoji, and markers may be either.
pub fn branded(mut option: Value, marker: &str) -> Value {
    let emote = marker
        .strip_prefix("<a:")
        .map(|emote| (true, emote))
        .or_else(|| marker.strip_prefix("<:").map(|emote| (false, emote)))
        .and_then(|(animated, emote)| Some((animated, emote.strip_suffix('>')?.split_once(':')?)));
    if let Some((animated, (name, id))) = emote {
        option["emoji"] = json!({ "id": id, "name": name, "animated": animated });
    }
    option
}

pub fn button(custom_id: &str, label: &str, style: u8, disabled: bool) -> Value {
    json!({
        "type": 2,
        "style": style,
        "custom_id": custom_id,
        "label": label,
        "disabled": disabled,
    })
}

/// Display text within Discord's limit; identity is carried by fingerprints,
/// so shortening here loses nothing.
pub fn text(value: &str) -> String {
    if value.is_empty() {
        return "(unnamed)".to_owned();
    }
    value.chars().take(TEXT_LIMIT).collect()
}

/// Bound free-form text, marking what was cut.
pub fn shorten(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.to_owned()
    } else {
        value.chars().take(limit - 1).chain(['…']).collect()
    }
}
