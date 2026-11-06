use anyhow::Result;
use carillon_installation::edit::edited;
use toml::{Table, Value};

use super::{DiscordConfig, ModuleSetting};

pub(super) fn text(settings: &DiscordConfig, file: Option<&str>) -> Result<String> {
    let Some((file, previous)) =
        file.and_then(|file| Some((file, toml::from_str::<DiscordConfig>(file).ok()?)))
    else {
        return Ok(toml::to_string_pretty(settings)?);
    };
    let unstated = Table::from_iter([(
        "module".to_owned(),
        Value::Table(Table::try_from(ModuleSetting::default())?),
    )]);
    let stated = |settings: &DiscordConfig| -> Result<Table> {
        let mut table = Table::try_from(settings)?;
        for (key, value) in &unstated {
            table.entry(key).or_insert_with(|| value.clone());
        }
        Ok(table)
    };
    match edited(
        file,
        &stated(&previous)?,
        &stated(settings)?,
        "channel_id",
        &unstated,
    ) {
        Ok(text) => Ok(text),
        Err(_) => Ok(toml::to_string_pretty(settings)?),
    }
}
