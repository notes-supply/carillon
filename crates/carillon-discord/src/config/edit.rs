use anyhow::Result;
use toml::{Table, Value};
use toml_edit::{
    Array, ArrayOfTables, Decor, DocumentMut, InlineTable, Item, RawString, TableLike,
};

use super::{DiscordConfig, ModuleSetting};

pub(super) fn text(settings: &DiscordConfig, file: Option<&str>) -> Result<String> {
    let Some((file, mut document, previous)) = file.and_then(|file| {
        let previous = toml::from_str::<DiscordConfig>(file).ok()?;
        Some((
            file,
            file.replace("\r\n", "\n").parse::<DocumentMut>().ok()?,
            previous,
        ))
    }) else {
        return Ok(toml::to_string_pretty(settings)?);
    };
    let skipped_defaults = Table::from_iter([(
        "module".to_owned(),
        Value::Table(Table::try_from(ModuleSetting::default())?),
    )]);
    let stated = |settings: &DiscordConfig| -> Result<Table> {
        let mut table = Table::try_from(settings)?;
        for (key, value) in &skipped_defaults {
            table.entry(key).or_insert_with(|| value.clone());
        }
        Ok(table)
    };
    edit(
        document.as_table_mut(),
        &stated(&previous)?,
        &stated(settings)?,
        Some(&skipped_defaults),
    );
    let mut text = document.to_string();
    if !file.ends_with('\n') && text.ends_with('\n') {
        text.pop();
    }
    Ok(if file.contains("\r\n") {
        text.replace('\n', "\r\n")
    } else {
        text
    })
}

fn edit(
    file: &mut dyn TableLike,
    previous: &Table,
    next: &Table,
    skipped_defaults: Option<&Table>,
) {
    for key in previous.keys().filter(|key| !next.contains_key(*key)) {
        file.remove(key);
    }
    for (key, after) in next {
        let before = previous.get(key);
        if before == Some(after) {
            continue;
        }
        let Some(item) = file.get_mut(key) else {
            file.insert(key, fresh_item(after));
            continue;
        };
        edit_item(item, before, after);
        let left_as_a_new_file_would = skipped_defaults
            .is_some_and(|defaults| defaults.get(key) == Some(after))
            && !commented(file, key);
        if left_as_a_new_file_would {
            file.remove(key);
        }
    }
}

fn edit_item(item: &mut Item, before: Option<&Value>, after: &Value) {
    match (before, after) {
        (Some(Value::Table(before)), Value::Table(after)) => {
            if let Some(table) = item.as_table_like_mut() {
                return edit(table, before, after, None);
            }
        }
        (Some(Value::Array(before)), Value::Array(after)) => {
            let edited = match item {
                Item::ArrayOfTables(tables) => edit_tables(tables, before, after),
                Item::Value(toml_edit::Value::Array(array)) => edit_array(array, before, after),
                _ => false,
            };
            if edited {
                return;
            }
        }
        _ => {}
    }
    match item {
        Item::Value(value) => replace(value, after),
        item => *item = fresh_item(after),
    }
}

struct Steps<'a> {
    removed_from_the_end: Vec<usize>,
    placed: Vec<Placed<'a>>,
}

enum Placed<'a> {
    Kept {
        index: usize,
        before: &'a Value,
        after: &'a Value,
    },
    Inserted(usize, &'a Value),
}

fn identity(value: &Value) -> &Value {
    value.get("channel_id").unwrap_or(value)
}

fn steps<'a>(before: &'a [Value], after: &'a [Value]) -> Option<Steps<'a>> {
    let among = |value: &Value, values: &[Value]| {
        values
            .iter()
            .any(|other| identity(other) == identity(value))
    };
    let removed_from_the_end = (0..before.len())
        .rev()
        .filter(|index| !among(&before[*index], after))
        .collect();
    let mut placed = Vec::new();
    let mut kept = before.iter().filter(|value| among(value, after)).peekable();
    for (index, value) in after.iter().enumerate() {
        match kept.peek() {
            Some(before) if identity(before) == identity(value) => {
                placed.push(Placed::Kept {
                    index,
                    before,
                    after: value,
                });
                kept.next();
            }
            _ if among(value, before) => return None,
            _ => placed.push(Placed::Inserted(index, value)),
        }
    }
    kept.next().is_none().then_some(Steps {
        removed_from_the_end,
        placed,
    })
}

fn edit_tables(file: &mut ArrayOfTables, before: &[Value], after: &[Value]) -> bool {
    let Some(steps) = steps(before, after)
        .filter(|_| file.len() == before.len() && after.iter().all(Value::is_table))
    else {
        return false;
    };
    let mut unplaced = String::new();
    for index in steps.removed_from_the_end {
        let removed = file.remove(index);
        let section = section_above(raw(removed.decor().prefix()));
        match file.get_mut(index) {
            Some(next) => prepend(next, section),
            None => unplaced.insert_str(0, section),
        }
    }
    let after_the_kept = steps
        .placed
        .iter()
        .rev()
        .find_map(|placed| match placed {
            Placed::Kept { index, .. } => Some(index + 1),
            Placed::Inserted(..) => None,
        })
        .unwrap_or(0);
    for placed in steps.placed {
        match placed {
            Placed::Inserted(index, Value::Table(table)) => file.insert(index, fresh_table(table)),
            Placed::Kept {
                index,
                before: Value::Table(before),
                after: Value::Table(after),
            } => {
                if let Some(table) = file.get_mut(index) {
                    edit(table, before, after, None);
                }
            }
            Placed::Inserted(..) | Placed::Kept { .. } => {}
        }
    }
    if let Some(next) = file.get_mut(after_the_kept) {
        prepend(next, &unplaced);
    }
    true
}

/// The comments a blank line separates from what follows them, through that
/// blank line: they head a section of the file, not the element below.
fn section_above(prefix: &str) -> &str {
    let end = prefix
        .split_inclusive('\n')
        .scan(0, |end, line| {
            *end += line.len();
            Some((*end, line))
        })
        .filter(|(_, line)| line.ends_with('\n') && line.trim().is_empty())
        .last()
        .map_or(0, |(end, _)| end);
    let (section, _) = prefix.split_at(end);
    if section.contains('#') { section } else { "" }
}

fn prepend(table: &mut toml_edit::Table, section: &str) {
    if !section.is_empty() {
        let own = table
            .decor()
            .prefix()
            .and_then(RawString::as_str)
            .unwrap_or("\n");
        let prefix = format!("{section}{}", own.strip_prefix('\n').unwrap_or(own));
        table.decor_mut().set_prefix(prefix);
    }
}

fn edit_array(file: &mut Array, before: &[Value], after: &[Value]) -> bool {
    let Some(steps) = steps(before, after).filter(|_| file.len() == before.len()) else {
        return false;
    };
    for index in steps.removed_from_the_end {
        remove_element(file, index);
    }
    for placed in steps.placed {
        match placed {
            Placed::Inserted(index, value) => insert_element(file, index, fresh_value(value)),
            Placed::Kept {
                index,
                before: Value::Table(before),
                after: Value::Table(after),
            } => {
                if let Some(table) = file
                    .get_mut(index)
                    .and_then(toml_edit::Value::as_inline_table_mut)
                {
                    edit(table, before, after, None);
                }
            }
            Placed::Kept { .. } => {}
        }
    }
    true
}

// An element's prefix holds the rest of the line before it, through its
// newline, then the element's own lines up to it. The text after the last
// element is the trailing text, or its suffix without a trailing comma.

fn remove_element(array: &mut Array, index: usize) {
    let last = index + 1 == array.len();
    let removed = array.remove(index);
    let prefix = raw(removed.decor().prefix());
    let following = if !last {
        prefix_of(array, index)
    } else if array.trailing_comma() {
        trailing(array)
    } else {
        raw(removed.decor().suffix()).to_owned()
    };
    let joined = left_by_removal(prefix, &following, index == 0, last);
    if !last {
        set_prefix(array, index, joined);
    } else if array.trailing_comma() {
        array.set_trailing(joined);
    } else if let Some(previous) = index.checked_sub(1).and_then(|index| array.get_mut(index)) {
        previous.decor_mut().set_suffix(joined);
    }
}

fn left_by_removal(removed_prefix: &str, following: &str, first: bool, last: bool) -> String {
    let (end_of_line_before, own_lines) = split_after_newline(removed_prefix);
    let (end_of_own_line, rest) = split_after_newline(following);
    if end_of_line_before.is_empty() && !first {
        following.to_owned()
    } else if end_of_own_line.is_empty() && !last {
        removed_prefix.to_owned()
    } else {
        format!("{end_of_line_before}{}{rest}", section_above(own_lines))
    }
}

fn insert_element(array: &mut Array, index: usize, mut value: toml_edit::Value) {
    let Some(last) = array.len().checked_sub(1) else {
        array.push(value);
        return;
    };
    if index <= last {
        let next = prefix_of(array, index);
        let (line, rest) = split_after_newline(&next);
        let (own, next) = match (line.is_empty(), index) {
            (true, 0) => (next.clone(), " ".to_owned()),
            (true, _) => (" ".to_owned(), " ".to_owned()),
            (false, _) => (format!("{line}{}", indent(&next)), format!("\n{rest}")),
        };
        set_prefix(array, index, next);
        value.decor_mut().set_prefix(own);
        value.decor_mut().set_suffix("");
        array.insert_formatted(index, value);
        return;
    }
    let previous = prefix_of(array, last);
    let following = if array.trailing_comma() {
        trailing(array)
    } else {
        array
            .get_mut(last)
            .map(|last| {
                let suffix = raw(last.decor().suffix()).to_owned();
                last.decor_mut().set_suffix("");
                suffix
            })
            .unwrap_or_default()
    };
    let (line, rest) = split_after_newline(&following);
    let (own, following) = match (line.is_empty(), previous.contains('\n')) {
        (true, false) => (" ".to_owned(), following.clone()),
        (true, true) => (format!("\n{}", indent(&previous)), following.clone()),
        (false, _) => (format!("{line}{}", indent(&previous)), format!("\n{rest}")),
    };
    value.decor_mut().set_prefix(own);
    if array.trailing_comma() {
        value.decor_mut().set_suffix("");
        array.set_trailing(following);
    } else {
        value.decor_mut().set_suffix(following);
    }
    array.push_formatted(value);
}

fn prefix_of(array: &Array, index: usize) -> String {
    array.get(index).map_or_else(String::new, |value| {
        value
            .decor()
            .prefix()
            .and_then(RawString::as_str)
            .unwrap_or(if index == 0 { "" } else { " " })
            .to_owned()
    })
}

fn set_prefix(array: &mut Array, index: usize, prefix: String) {
    if let Some(value) = array.get_mut(index) {
        value.decor_mut().set_prefix(prefix);
    }
}

fn trailing(array: &Array) -> String {
    array.trailing().as_str().unwrap_or_default().to_owned()
}

fn raw(text: Option<&RawString>) -> &str {
    text.and_then(RawString::as_str).unwrap_or_default()
}

fn split_after_newline(text: &str) -> (&str, &str) {
    text.find('\n')
        .map_or(("", text), |end| text.split_at(end + 1))
}

fn indent(prefix: &str) -> &str {
    prefix.rsplit('\n').next().unwrap_or_default()
}

/// Whether a comment is on `key`'s line, above it or within its table. A
/// comment on the lines above a key is in the key's decor.
fn commented(table: &dyn TableLike, key: &str) -> bool {
    let Some((key, item)) = table.get_key_value(key) else {
        return false;
    };
    let decor = match item {
        Item::Table(table) => Some(table.decor()),
        Item::Value(value) => Some(value.decor()),
        Item::None | Item::ArrayOfTables(_) => None,
    };
    [key.leaf_decor(), key.dotted_decor()]
        .into_iter()
        .chain(decor)
        .any(has_comment)
        || item
            .as_table_like()
            .is_some_and(|table| table.iter().any(|(key, _)| commented(table, key)))
}

fn has_comment(decor: &Decor) -> bool {
    [decor.prefix(), decor.suffix()]
        .into_iter()
        .any(|text| raw(text).contains('#'))
}

fn replace(value: &mut toml_edit::Value, after: &Value) {
    let literal = match (&*value, after) {
        (toml_edit::Value::String(before), Value::String(after))
            if before
                .as_repr()
                .and_then(|repr| repr.as_raw().as_str())
                .is_some_and(|raw| raw.starts_with('\'')) =>
        {
            format!("'{after}'").parse().ok()
        }
        _ => None,
    };
    let mut new = literal.unwrap_or_else(|| fresh_value(after));
    *new.decor_mut() = value.decor().clone();
    *value = new;
}

fn fresh_item(value: &Value) -> Item {
    match value {
        Value::Table(table) => Item::Table(fresh_table(table)),
        Value::Array(tables) if !tables.is_empty() && tables.iter().all(Value::is_table) => {
            Item::ArrayOfTables(
                tables
                    .iter()
                    .filter_map(Value::as_table)
                    .map(fresh_table)
                    .collect(),
            )
        }
        value => Item::Value(fresh_value(value)),
    }
}

fn fresh_table(table: &Table) -> toml_edit::Table {
    table
        .iter()
        .map(|(key, value)| (key, fresh_item(value)))
        .collect()
}

fn fresh_value(value: &Value) -> toml_edit::Value {
    match value {
        Value::String(text) => text.into(),
        Value::Integer(number) => (*number).into(),
        Value::Float(number) => (*number).into(),
        Value::Boolean(flag) => (*flag).into(),
        Value::Datetime(datetime) => (*datetime).into(),
        Value::Array(values) => values.iter().map(fresh_value).collect::<Array>().into(),
        Value::Table(table) => table
            .iter()
            .map(|(key, value)| (key, fresh_value(value)))
            .collect::<InlineTable>()
            .into(),
    }
}
