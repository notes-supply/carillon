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
pub const PREVIOUS: &str = "previous";
pub const NEXT: &str = "next";

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

/// Two fewer than fit once options overflow, leaving room for [`paged`]'s turns.
const fn page_size(count: usize) -> usize {
    if count > PAGE { PAGE - 2 } else { PAGE }
}

pub const fn page_holding(index: usize, count: usize) -> usize {
    index / page_size(count)
}

/// One page of `options`, clamped to the last, with options valued
/// [`PREVIOUS`] and [`NEXT`] that turn to the pages beside it, so a select
/// never lists more than [`PAGE`]. Returns the page shown.
pub fn paged(options: Vec<Value>, page: usize) -> (usize, Vec<Value>) {
    let size = page_size(options.len());
    let pages = options.len().div_ceil(size).max(1);
    let page = page.min(pages - 1);
    let mut shown: Vec<Value> = options.into_iter().skip(page * size).take(size).collect();
    if page > 0 {
        shown.push(choice("◀ Previous", PREVIOUS, None, false));
    }
    if page + 1 < pages {
        shown.push(choice("Next ▶", NEXT, None, false));
    }
    (page, shown)
}

pub fn choice(label: &str, value: &str, description: Option<&str>, default: bool) -> Value {
    let mut option = json!({ "label": text(label), "value": value, "default": default });
    if let Some(description) = description {
        option["description"] = text(description).into();
    }
    option
}

/// The option with a custom emote or Unicode emoji as its badge. Other
/// configured symbols stay in text because Discord refuses them as emoji.
pub fn branded(mut option: Value, marker: &str) -> Value {
    let emote = marker
        .strip_circumfix("<a:", '>')
        .map(|emote| (true, emote))
        .or_else(|| Some((false, marker.strip_circumfix("<:", '>')?)))
        .and_then(|(animated, emote)| Some((animated, emote.split_once(':')?)));
    if let Some((animated, (name, id))) = emote {
        option["emoji"] = json!({ "id": id, "name": name, "animated": animated });
    } else if emojis::get(marker).is_some() {
        option["emoji"] = json!({ "name": marker });
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
