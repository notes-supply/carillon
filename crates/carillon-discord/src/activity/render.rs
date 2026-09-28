//! Pure Discord activity-card text; monitoring and snapshot retention live in
//! the activity lifecycle. Markers are validated provider configuration.
use crate::config::ActivityMarkers;
use carillon_protocol::model::{
    ActiveTurn, ActivityPhase, DeliveryOutcome, FinishedTool, TurnActivity, TurnState,
};

const RECENT_LINES: usize = 5;
const RUNNING_LINES: usize = 3;
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
        ActivityPhase::Model => " · Waiting for the model",
        ActivityPhase::Tools => "",
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
        }
    }
    cut(&lines.join("\n"), MESSAGE_LIMIT)
}

/// A tool as plain text and its detail as inline code, so neither can
/// format, mention or link anything.
fn tool_line(mark: &str, tool: &str, detail: Option<&str>) -> String {
    let tool = tool
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || "_.:-".contains(character) {
                character
            } else {
                '?'
            }
        })
        .collect::<String>();
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
