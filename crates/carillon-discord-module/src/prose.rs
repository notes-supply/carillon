//! Text a model wrote, shown with its inline emphasis: `**bold**`,
//! `*italic*` or `_italic_`, and `` `code` `` render when they open and
//! close on one line. Everything else is escaped, so the text never
//! mentions, links, quotes, lists, fences, hides, heads a section, or shows
//! a custom emoji or a timestamp. Cut text before rendering it, so a cut
//! span is unbalanced and stays escaped.

use crate::embed::escaped;

/// What one line of model text renders as.
enum Part<'a> {
    /// Shown escaped.
    Text(&'a str),
    /// Inline code, which Discord shows literally.
    Code(&'a str),
    Bold(Vec<Part<'a>>),
    Italic(char, Vec<Part<'a>>),
}

pub fn prose(text: &str) -> String {
    text.split('\n').map(line).collect::<Vec<_>>().join("\n")
}

fn line(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    render(&parse(text), &mut shown);
    // A line that starts with a list marker would be a list.
    let start = shown.len() - shown.trim_start().len();
    let digits = shown[start..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    match shown[start + digits..].chars().next() {
        Some('-' | '+') if digits == 0 => shown.insert(start, '\\'),
        Some('.' | ')') if digits > 0 => shown.insert(start + digits, '\\'),
        _ => {}
    }
    shown
}

fn render(parts: &[Part<'_>], shown: &mut String) {
    for part in parts {
        match part {
            Part::Text(text) => shown.push_str(&escaped(text)),
            Part::Code(code) => {
                shown.push('`');
                shown.push_str(code);
                shown.push('`');
            }
            Part::Bold(inner) => {
                shown.push_str("**");
                render(inner, shown);
                shown.push_str("**");
            }
            Part::Italic(marker, inner) => {
                shown.push(*marker);
                render(inner, shown);
                shown.push(*marker);
            }
        }
    }
}

fn parse(text: &str) -> Vec<Part<'_>> {
    let mut parts = Vec::new();
    let (mut plain, mut at) = (0, 0);
    while let Some(next) = text[at..].chars().next() {
        if next == '\\' {
            at += escape(text, at);
            continue;
        }
        let span = match next {
            '`' => code(text, at),
            '*' if text[at..].starts_with("**") => bold(text, at),
            '*' | '_' => italic(text, at, next),
            _ => None,
        };
        if let Some((part, end)) = span {
            if plain < at {
                parts.push(Part::Text(&text[plain..at]));
            }
            parts.push(part);
            (plain, at) = (end, end);
        } else {
            at += next.len_utf8();
        }
    }
    if plain < text.len() {
        parts.push(Part::Text(&text[plain..]));
    }
    parts
}

/// A code span opening at `at` and the end of its closing backtick. Spans
/// touching another backtick are refused, as Discord would join them.
fn code(text: &str, at: usize) -> Option<(Part<'_>, usize)> {
    let close = at + 1 + text[at + 1..].find('`')?;
    let inner = &text[at + 1..close];
    let touching = text[..at].ends_with('`') || text[close + 1..].starts_with('`');
    (!inner.is_empty() && !touching).then(|| (Part::Code(inner), close + 1))
}

/// Bold opening at `at`, closed by the next `**` on the line.
fn bold(text: &str, at: usize) -> Option<(Part<'_>, usize)> {
    if text[..at].ends_with('*') {
        return None;
    }
    let close = closing(text, at + 2, "**")?;
    let inner = &text[at + 2..close];
    (padded(inner) && !text[close + 2..].starts_with('*'))
        .then(|| (Part::Bold(parse(inner)), close + 2))
}

/// Italic opening at `at` with `marker`, closed by the next one on the line.
/// An underscore must stand at a word's edge, so `snake_case` stays text.
fn italic(text: &str, at: usize, marker: char) -> Option<(Part<'_>, usize)> {
    let edge = |character: Option<char>| {
        marker == '*'
            || !character.is_some_and(|character| character.is_alphanumeric() || character == '_')
    };
    if text[..at].ends_with(marker) || !edge(text[..at].chars().next_back()) {
        return None;
    }
    let close = closing(text, at + 1, marker.encode_utf8(&mut [0; 4]))?;
    let inner = &text[at + 1..close];
    let after = text[close + 1..].chars().next();
    (padded(inner) && after != Some(marker) && edge(after))
        .then(|| (Part::Italic(marker, parse(inner)), close + 1))
}

/// Where `marker` next closes a span from `from`. A marker the model escaped
/// with a backslash closes nothing.
fn closing(text: &str, from: usize, marker: &str) -> Option<usize> {
    let mut at = from;
    while let Some(next) = text[at..].chars().next() {
        if next == '\\' {
            at += escape(text, at);
        } else if text[at..].starts_with(marker) {
            return Some(at);
        } else {
            at += next.len_utf8();
        }
    }
    None
}

/// The length of the backslash at `at` and of the character it escapes.
fn escape(text: &str, at: usize) -> usize {
    1 + text[at + 1..].chars().next().map_or(0, char::len_utf8)
}

/// Emphasis needs text that neither starts nor ends with whitespace.
fn padded(inner: &str) -> bool {
    !inner.is_empty() && inner.trim() == inner
}

#[cfg(test)]
mod tests;
