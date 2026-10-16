//! What the module answers, checked as Discord takes it, so a module that
//! exceeds Discord's limits is reported instead of discovered as a rejected
//! edit that leaves an acknowledgement hanging or a card stale.

use anyhow::{Context as _, Result, bail};
use carillon_discord_module::contract::Reply;
use serde_json::Value;
use twilight_model::channel::message::{Component, Embed, component::SelectMenuType};
use twilight_validate::component::{COMPONENT_COUNT, component_v1};

/// A reply as Discord takes it.
pub(crate) struct Answer {
    pub content: String,
    pub embeds: Vec<Embed>,
    pub components: Vec<Component>,
}

impl Answer {
    /// Removes menu badges Discord can refuse while keeping the text emotes.
    pub(crate) fn remove_option_emojis(&mut self) -> bool {
        let mut removed = false;
        for component in &mut self.components {
            if let Component::ActionRow(row) = component {
                for inner in &mut row.components {
                    if let Component::SelectMenu(menu) = inner {
                        for option in menu.options.iter_mut().flatten() {
                            removed |= option.emoji.take().is_some();
                        }
                    }
                }
            }
        }
        removed
    }

    /// Text alone, which as an edit also clears what the message showed.
    pub(crate) fn notice(content: &str) -> Self {
        Self {
            content: content.to_owned(),
            embeds: Vec::new(),
            components: Vec::new(),
        }
    }
}

/// The reply as Discord accepts it: content within its limit, embeds as a
/// card's embed is checked, and at most five action rows of buttons and
/// select menus within Discord's limits.
pub(crate) fn answer(reply: &Reply) -> Result<Answer> {
    twilight_validate::message::content(&reply.content)
        .context("Discord module reply has unusable content")?;
    let embeds =
        embeds(reply.embeds.clone()).context("Discord module reply has unusable embeds")?;
    let components: Vec<Component> = serde_json::from_value(Value::Array(reply.components.clone()))
        .context("Discord module reply has invalid components")?;
    if components.len() > COMPONENT_COUNT {
        bail!(
            "Discord module reply has {} action rows; Discord allows {COMPONENT_COUNT}",
            components.len()
        );
    }
    for component in &components {
        component_v1(component).context("Discord module reply has an unusable component")?;
        // twilight checks the maxima; Discord also refuses an empty row or an
        // empty select menu.
        let Component::ActionRow(row) = component else {
            bail!("Discord module reply has a component outside an action row");
        };
        if row.components.is_empty() {
            bail!("Discord module reply has an empty action row");
        }
        for inner in &row.components {
            if let Component::SelectMenu(menu) = inner
                && menu.kind == SelectMenuType::Text
                && menu.options.as_ref().is_none_or(Vec::is_empty)
            {
                bail!("Discord module reply has a select menu without options");
            }
        }
    }
    Ok(Answer {
        content: reply.content.clone(),
        embeds,
        components,
    })
}

/// Embeds in Discord's own JSON, as one message takes them.
pub(super) fn embeds(embeds: Vec<Value>) -> Result<Vec<Embed>> {
    let embeds: Vec<Embed> =
        serde_json::from_value(Value::Array(embeds)).context("not Discord embeds")?;
    twilight_validate::message::embeds(&embeds).context("beyond Discord's limits")?;
    Ok(embeds)
}

#[cfg(test)]
mod tests;
