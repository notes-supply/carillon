//! The settings file's text, written as an edit of what the owner wrote:
//! only the values a change makes differ are written, each in its place
//! and keeping its comment, so a change and its undo leave the file as it
//! was. Comments, order and the defaults the file leaves unstated stay.

use anyhow::Result;
use toml::{Table, Value};
use toml_edit::{ArrayOfTables, DocumentMut, Item, TableLike};

use super::DiscordConfig;

/// The text of a file that says `settings`: `file`, the text in place,
/// edited where it says otherwise, or a new file's text when there is none
/// or it is not settings.
pub(super) fn text(settings: &DiscordConfig, file: Option<&str>) -> Result<String> {
    let written = toml::to_string_pretty(settings)?;
    let Some((mut file, previous)) = file.and_then(|file| {
        let previous = toml::from_str::<DiscordConfig>(file).ok()?;
        Some((file.parse::<DocumentMut>().ok()?, previous))
    }) else {
        return Ok(written);
    };
    edit(
        file.as_table_mut(),
        written.parse::<DocumentMut>()?.as_table(),
        &Table::try_from(previous)?,
        &Table::try_from(settings)?,
    );
    Ok(file.to_string())
}

/// Edits `file`, which says `previous`, to say `next`. `written` is `next`
/// as a new file lays it out, and gives each value the edit writes.
fn edit(file: &mut dyn TableLike, written: &dyn TableLike, previous: &Table, next: &Table) {
    for key in previous.keys().filter(|key| !next.contains_key(*key)) {
        file.remove(key);
    }
    for (key, after) in next {
        let before = previous.get(key);
        if before == Some(after) {
            continue;
        }
        let Some(new) = written.get(key) else {
            continue;
        };
        let Some(item) = file.get_mut(key) else {
            file.insert(key, new.clone());
            continue;
        };
        match (before, after) {
            (Some(Value::Table(before)), Value::Table(after)) => {
                if let (Some(table), Some(new)) = (item.as_table_like_mut(), new.as_table_like()) {
                    edit(table, new, before, after);
                    continue;
                }
            }
            (Some(Value::Array(before)), Value::Array(after)) => {
                if let (Some(tables), Some(new)) =
                    (item.as_array_of_tables_mut(), new.as_array_of_tables())
                    && tables.len() == before.len()
                {
                    edit_tables(tables, new, before, after);
                    continue;
                }
            }
            _ => {}
        }
        replace(item, new);
    }
}

/// Edits each table of `file` in place, adding or removing tables at the
/// end as the count changes.
fn edit_tables(
    file: &mut ArrayOfTables,
    written: &ArrayOfTables,
    previous: &[Value],
    next: &[Value],
) {
    while file.len() > next.len() {
        file.remove(file.len() - 1);
    }
    for (index, after) in next.iter().enumerate() {
        let Some(new) = written.get(index) else {
            continue;
        };
        match (file.get_mut(index), previous.get(index), after) {
            (Some(table), Some(Value::Table(before)), Value::Table(after)) => {
                edit(table, new, before, after);
            }
            (Some(table), _, _) => *table = new.clone(),
            (None, _, _) => file.push(new.clone()),
        }
    }
}

/// Puts `new` in place of `item`. A value keeps its spacing and comment,
/// and stays a value inside an inline table.
fn replace(item: &mut Item, new: &Item) {
    match (item.as_value_mut(), new.clone().into_value()) {
        (Some(old), Ok(mut new)) => {
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
        _ => *item = new.clone(),
    }
}
