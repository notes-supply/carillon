//! Embeds as Discord's JSON names their parts, and the text that work cards
//! and commands share.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Embed {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
}

#[derive(Debug, Serialize)]
pub struct Field {
    pub inline: bool,
    pub name: String,
    pub value: String,
}

/// A model and what it runs with, as one line: its brand, then each part
/// joined by ` · `.
pub fn model_line(brand: &str, parts: &[String]) -> String {
    marked(brand, &parts.join(" · "))
}

/// Empty markers omit their separating space too.
pub fn marked(marker: &str, text: &str) -> String {
    if marker.is_empty() {
        text.to_owned()
    } else {
        format!("{marker} {text}")
    }
}

/// Text shown as written but unable to format, mention or link anything.
pub fn escaped(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\*_~`|<>[]()#:@".contains(character) {
            shown.push('\\');
        }
        shown.push(character);
    }
    shown
}
