//! Pure Discord activity-card text; monitoring and snapshot retention live in
//! the activity lifecycle. Markers are validated provider configuration.
use crate::config::ActivityMarkers;
use carillon_protocol::model::{
    ActiveTurn, ActivityPhase, DeliveryOutcome, FinishedTool, TurnActivity, TurnState, WorkItem,
    WorkKind, WorkState, WorkSummary,
};
use std::fmt::Write as _;

const RECENT_LINES: usize = 5;
const RUNNING_LINES: usize = 3;
const WORK_LINES: usize = 4;
const LABEL_LIMIT: usize = 60;
const DETAIL_LIMIT: usize = 120;
const HEADLINE_LIMIT: usize = 100;
const MESSAGE_LIMIT: usize = 2_000;

/// A running or queued turn's card. Without a current snapshot, keep only
/// recent history from the last snapshot, never anything still running.
pub(super) fn active_text(
    turn: &ActiveTurn,
    last: Option<&TurnActivity>,
    markers: &ActivityMarkers,
) -> String {
    let started = format!("<t:{}:R>", turn.accepted_at);
    if turn.state == TurnState::Queued {
        return marked(
            &markers.queued,
            &format!("Waiting for earlier work to finish · queued {started}"),
        );
    }
    let Some(activity) = &turn.activity else {
        return card_text(
            &marked(&markers.working, &format!("Working · started {started}")),
            last,
            false,
            markers,
        );
    };
    let label = match activity.phase {
        ActivityPhase::Starting => " · Starting",
        ActivityPhase::Queued => " · Queued behind other work in this conversation",
        ActivityPhase::Model => " · Waiting for the model",
        ActivityPhase::Tools => "",
        ActivityPhase::Waiting => " · Waiting",
        ActivityPhase::Retrying => " · Retrying the model request",
        ActivityPhase::Compacting => " · Compacting the conversation",
    };
    card_text(
        &marked(
            &markers.working,
            &format!("Working{label} · started {started}"),
        ),
        Some(activity),
        true,
        markers,
    )
}

/// Core has stopped listing the turn: it no longer runs, and its answer or
/// notice is on its way. Nothing is shown as still running.
pub(super) fn finishing_text(last: Option<&TurnActivity>, markers: &ActivityMarkers) -> String {
    card_text(
        &marked(&markers.finishing, "Finishing up"),
        last,
        false,
        markers,
    )
}

/// What a card says after its turn ended, or `None` when it is removed.
pub(super) fn final_text(
    turn_state: TurnState,
    delivery: DeliveryOutcome,
    last: Option<&TurnActivity>,
    markers: &ActivityMarkers,
) -> Option<String> {
    let (marker, status) = match (turn_state, delivery) {
        (TurnState::Completed, DeliveryOutcome::Delivered) => return None,
        (TurnState::Completed, DeliveryOutcome::Uncertain) => (
            &markers.uncertain,
            "Finished, but the reply may not have been delivered.",
        ),
        (_, DeliveryOutcome::Delivered) => (&markers.stopped, "Stopped before finishing."),
        (_, DeliveryOutcome::Uncertain) => (
            &markers.stopped,
            "Stopped before finishing; the notice about it may not have been delivered.",
        ),
    };
    Some(card_text(&marked(marker, status), last, false, markers))
}

fn card_text(
    header: &str,
    activity: Option<&TurnActivity>,
    running: bool,
    markers: &ActivityMarkers,
) -> String {
    let mut lines = vec![header.to_owned()];
    if let Some(activity) = activity {
        if running && let Some(headline) = &activity.headline {
            lines.push(format!("> {}", cut(headline, HEADLINE_LIMIT)));
        }
        let shown_running = if running { activity.running.len() } else { 0 };
        let earlier = usize::try_from(activity.tool_calls)
            .unwrap_or(usize::MAX)
            .saturating_sub(activity.recent.len() + shown_running);
        if earlier > 0 {
            lines.push(format!(
                "… {earlier} earlier call{}",
                if earlier == 1 { "" } else { "s" }
            ));
        }
        let recent = activity.recent.len().saturating_sub(RECENT_LINES);
        for FinishedTool { tool, detail, ok } in &activity.recent[recent..] {
            lines.push(tool_line(
                if *ok {
                    &markers.succeeded
                } else {
                    &markers.failed
                },
                tool,
                detail.as_deref(),
            ));
        }
        if running {
            for tool in activity.running.iter().take(RUNNING_LINES) {
                lines.push(format!(
                    "{} · <t:{}:R>",
                    tool_line(&markers.running, &tool.tool, tool.detail.as_deref()),
                    tool.started_at
                ));
            }
            let more = activity.running.len().saturating_sub(RUNNING_LINES);
            if more > 0 {
                lines.push(marked(
                    &markers.running,
                    &format!("… and {more} more running"),
                ));
            }
            if let Some(work) = &activity.work {
                work_lines(&mut lines, work, markers);
            }
        }
    }
    cut(&lines.join("\n"), MESSAGE_LIMIT)
}

/// Delegated and transferred work, one bounded line per item. A known
/// parent indents its children; items beyond the shown lines and an exact
/// further count become "… and N more". Incomplete observation is said as
/// such beside that count, which covers only what is known, and never
/// becomes a guessed total.
fn work_lines(lines: &mut Vec<String>, work: &WorkSummary, markers: &ActivityMarkers) {
    let shown = &work.items[..work.items.len().min(WORK_LINES)];
    for item in shown {
        let nested = item
            .parent
            .as_ref()
            .is_some_and(|parent| shown.iter().any(|other| &other.id == parent));
        let line = work_line(item, markers);
        lines.push(if nested {
            format!("\u{2003}{line}")
        } else {
            line
        });
    }
    let more = work.items.len() - shown.len()
        + work.omitted.map_or(0, |omitted| {
            usize::try_from(omitted.get()).unwrap_or(usize::MAX)
        });
    if more > 0 {
        lines.push(marked(&markers.queued, &format!("… and {more} more")));
    }
    if work.partial {
        lines.push(marked(
            &markers.uncertain,
            "… additional work not fully observed",
        ));
    }
}

fn work_line(item: &WorkItem, markers: &ActivityMarkers) -> String {
    let marker = match item.state {
        WorkState::Queued | WorkState::Waiting => &markers.queued,
        WorkState::Running => &markers.running,
        WorkState::Succeeded => &markers.succeeded,
        WorkState::Failed => &markers.failed,
        WorkState::Cancelled => &markers.stopped,
        WorkState::Unknown => &markers.uncertain,
    };
    let kind = match item.kind {
        WorkKind::Task => "task",
        WorkKind::Delegation => "delegation",
        WorkKind::Handoff => "handoff",
        WorkKind::Fork => "fork",
    };
    let state = match item.state {
        WorkState::Queued => "queued",
        WorkState::Running => "running",
        WorkState::Waiting => "waiting",
        WorkState::Succeeded => "done",
        WorkState::Failed => "failed",
        WorkState::Cancelled => "stopped",
        WorkState::Unknown => "state unknown",
    };
    let label = cut(&item.label.replace('`', "ˋ"), LABEL_LIMIT);
    let mut text = format!("{kind} `{label}` · {state}");
    if let Some(actor) = &item.actor {
        text.push_str(" · ");
        text.push_str(&plain(&cut(actor, LABEL_LIMIT)));
    }
    if item.state == WorkState::Running
        && let Some(started_at) = item.started_at
    {
        let _ = write!(text, " · <t:{started_at}:R>");
    }
    marked(marker, &text)
}

/// Text that cannot format, mention or link anything: letters, digits and
/// a few separators, everything else replaced.
fn plain(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_alphanumeric() || "_.:-".contains(character) {
                character
            } else {
                '?'
            }
        })
        .collect()
}

/// A tool as plain text and its detail as inline code, so neither can
/// format, mention or link anything.
fn tool_line(mark: &str, tool: &str, detail: Option<&str>) -> String {
    let tool = plain(tool);
    let text = match detail {
        Some(detail) => format!("{tool} `{}`", cut(&detail.replace('`', "ˋ"), DETAIL_LIMIT)),
        None => tool,
    };
    marked(mark, &text)
}

/// Empty markers omit their separating space too.
fn marked(marker: &str, text: &str) -> String {
    if marker.is_empty() {
        text.to_owned()
    } else {
        format!("{marker} {text}")
    }
}

fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit - 1) {
        Some((end, _)) if text[end..].chars().count() > 1 => format!("{}…", &text[..end]),
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests;
