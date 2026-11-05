//! The settings file's text, written as an edit of what the owner wrote:
//! only the values a change makes differ are written, each in its place
//! and keeping its comment, so a change and its undo leave the file as it
//! was. Comments, order, line endings and the defaults the file leaves
//! unstated stay. Routes are matched by channel and other array elements
//! by value, so each keeps its line and comments.

use anyhow::Result;
use toml::{Table, Value};
use toml_edit::{
    Array, ArrayOfTables, Decor, DocumentMut, InlineTable, Item, RawString, TableLike,
};

use super::{DiscordConfig, ModuleSetting};

/// The text of a file that says `settings`: `file`, the text in place,
/// edited where it says otherwise, or a new file's text when there is none
/// or it is not settings.
pub(super) fn text(settings: &DiscordConfig, file: Option<&str>) -> Result<String> {
    let Some((mut document, previous)) = file.and_then(|file| {
        let previous = toml::from_str::<DiscordConfig>(file).ok()?;
        Some((
            file.replace("\r\n", "\n").parse::<DocumentMut>().ok()?,
            previous,
        ))
    }) else {
        return Ok(toml::to_string_pretty(settings)?);
    };
    // Settings a new file leaves unstated, spelt out as a file may state them.
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
    edit(
        document.as_table_mut(),
        &stated(&previous)?,
        &stated(settings)?,
        Some(&unstated),
    );
    let text = document.to_string();
    Ok(if file.is_some_and(|file| file.contains("\r\n")) {
        text.replace('\n', "\r\n")
    } else {
        text
    })
}

/// Edits `file`, which says `previous`, to say `next`. A value or table
/// that comes to say what `unstated` holds for it goes, as a new file
/// leaves it unstated, unless a comment remains on it.
fn edit(file: &mut dyn TableLike, previous: &Table, next: &Table, unstated: Option<&Table>) {
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
        let unstated = unstated.and_then(|unstated| unstated.get(key));
        edit_item(item, before, after, unstated.and_then(Value::as_table));
        if unstated == Some(after) && !commented(item) {
            file.remove(key);
        }
    }
}

fn edit_item(item: &mut Item, before: Option<&Value>, after: &Value, unstated: Option<&Table>) {
    match (before, after) {
        (Some(Value::Table(before)), Value::Table(after)) => {
            if let Some(table) = item.as_table_like_mut() {
                return edit(table, before, after, unstated);
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

/// One edit of an array's elements: removals come first, from the end,
/// then each element of the new array in order, kept or inserted.
enum Step<'a> {
    Remove(usize),
    Keep {
        index: usize,
        before: &'a Value,
        after: &'a Value,
    },
    Insert(usize, &'a Value),
}

/// What an element is known by: a route by its channel, any other element
/// by its value.
fn identity(value: &Value) -> &Value {
    value.get("channel_id").unwrap_or(value)
}

/// The steps that turn the elements `before` into `after`, or none when
/// `after` reorders the elements it keeps.
fn steps<'a>(before: &'a [Value], after: &'a [Value]) -> Option<Vec<Step<'a>>> {
    let among = |value: &Value, values: &[Value]| {
        values
            .iter()
            .any(|other| identity(other) == identity(value))
    };
    let mut steps: Vec<Step<'_>> = (0..before.len())
        .rev()
        .filter(|index| !among(&before[*index], after))
        .map(Step::Remove)
        .collect();
    let mut kept = before.iter().filter(|value| among(value, after)).peekable();
    for (index, value) in after.iter().enumerate() {
        match kept.peek() {
            Some(before) if identity(before) == identity(value) => {
                steps.push(Step::Keep {
                    index,
                    before,
                    after: value,
                });
                kept.next();
            }
            _ if among(value, before) => return None,
            _ => steps.push(Step::Insert(index, value)),
        }
    }
    kept.next().is_none().then_some(steps)
}

/// Edits each `[[table]]` in place, matched as [`steps`] matches them.
fn edit_tables(file: &mut ArrayOfTables, before: &[Value], after: &[Value]) -> bool {
    let Some(steps) = steps(before, after)
        .filter(|_| file.len() == before.len() && after.iter().all(Value::is_table))
    else {
        return false;
    };
    for step in steps {
        match step {
            Step::Remove(index) => {
                file.remove(index);
            }
            Step::Insert(index, Value::Table(table)) => file.insert(index, fresh_table(table)),
            Step::Keep {
                index,
                before: Value::Table(before),
                after: Value::Table(after),
            } => {
                if let Some(table) = file.get_mut(index) {
                    edit(table, before, after, None);
                }
            }
            Step::Insert(..) | Step::Keep { .. } => {}
        }
    }
    true
}

/// Edits each element in place, matched as [`steps`] matches them, so each
/// keeps its line and the comments on it.
fn edit_array(file: &mut Array, before: &[Value], after: &[Value]) -> bool {
    let Some(steps) = steps(before, after).filter(|_| file.len() == before.len()) else {
        return false;
    };
    for step in steps {
        match step {
            Step::Remove(index) => remove_element(file, index),
            Step::Insert(index, value) => insert_element(file, index, fresh(value)),
            Step::Keep {
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
            Step::Keep { .. } => {}
        }
    }
    true
}

// An element's prefix holds the rest of the line before it, through its
// newline, then the element's own lines up to it. The text after the last
// element is the trailing text, or its suffix without a trailing comma.

/// Removes the element at `index` with its own line: the comment that ends
/// the line before stays, and the one that ends its own line goes.
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
    let (line, _) = split_line(prefix);
    let (rest_of_line, rest) = split_line(&following);
    let joined = if rest_of_line.is_empty() && index == 0 {
        prefix.to_owned()
    } else {
        format!("{line}{rest}")
    };
    if !last {
        set_prefix(array, index, joined);
    } else if array.trailing_comma() {
        array.set_trailing(joined);
    } else if let Some(previous) = index.checked_sub(1).and_then(|index| array.get_mut(index)) {
        previous.decor_mut().set_suffix(joined);
    }
}

/// Inserts `value` at `index`: on a line of its own, indented like its
/// neighbour, when the array lists one element a line.
fn insert_element(array: &mut Array, index: usize, mut value: toml_edit::Value) {
    let Some(last) = array.len().checked_sub(1) else {
        array.push(value);
        return;
    };
    if index <= last {
        let next = prefix_of(array, index);
        let (line, rest) = split_line(&next);
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
    let (line, rest) = split_line(&following);
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

/// `text` split after its first newline; all of it is the second part when
/// it has none.
fn split_line(text: &str) -> (&str, &str) {
    text.find('\n')
        .map_or(("", text), |end| text.split_at(end + 1))
}

/// The indentation that ends `prefix`.
fn indent(prefix: &str) -> &str {
    prefix.rsplit('\n').next().unwrap_or_default()
}

/// Whether the owner commented `item`, or any value in it.
fn commented(item: &Item) -> bool {
    let decor = match item {
        Item::Table(table) => Some(table.decor()),
        Item::Value(value) => Some(value.decor()),
        Item::None | Item::ArrayOfTables(_) => None,
    };
    decor.is_some_and(has_comment)
        || item.as_table_like().is_some_and(|table| {
            table.iter().any(|(key, item)| {
                table
                    .key(key)
                    .is_some_and(|key| has_comment(key.leaf_decor()))
                    || commented(item)
            })
        })
}

fn has_comment(decor: &Decor) -> bool {
    [decor.prefix(), decor.suffix()]
        .into_iter()
        .any(|text| raw(text).contains('#'))
}

/// Writes `after` in place of `value`, keeping its spacing and comment, and
/// its single quotes when `after` can be written in them.
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
    let mut new = literal.unwrap_or_else(|| fresh(after));
    *new.decor_mut() = value.decor().clone();
    *value = new;
}

/// `value` as a new file writes it inside a table.
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
        value => Item::Value(fresh(value)),
    }
}

fn fresh_table(table: &Table) -> toml_edit::Table {
    table
        .iter()
        .map(|(key, value)| (key, fresh_item(value)))
        .collect()
}

/// `value` as a new file writes it inside a value.
fn fresh(value: &Value) -> toml_edit::Value {
    match value {
        Value::String(text) => text.into(),
        Value::Integer(number) => (*number).into(),
        Value::Float(number) => (*number).into(),
        Value::Boolean(flag) => (*flag).into(),
        Value::Datetime(datetime) => (*datetime).into(),
        Value::Array(values) => values.iter().map(fresh).collect::<Array>().into(),
        Value::Table(table) => table
            .iter()
            .map(|(key, value)| (key, fresh(value)))
            .collect::<InlineTable>()
            .into(),
    }
}
