//! Pure Discord work-card embeds: a job's status, the reasoning its runtime
//! published, its calls, its delegated work and its agent, within Discord's
//! embed limits. Monitoring and retention live in the activity lifecycle.
//! Markers are validated provider configuration, shown on status/model rows;
//! section names stay plain. Text a model wrote cannot format, mention or
//! link anything.
use crate::config::ActivityMarkers;
use carillon_protocol::model::{
    ActivityPhase, AgentInfo, Delivery, DeliveryOutcome, FinishedTool, TurnActivity, TurnState,
    WorkItem, WorkKind, WorkState,
};
use std::{collections::HashMap, fmt::Write as _};
use twilight_model::channel::message::{Embed, embed::EmbedField};
use twilight_validate::embed::{DESCRIPTION_LENGTH, FIELD_VALUE_LENGTH};

const RECENT_LINES: usize = 5;
const RUNNING_LINES: usize = 3;
const WORK_LINES: usize = 6;
const LABEL_LIMIT: usize = 60;
const DETAIL_LIMIT: usize = 120;
const HEADLINE_LIMIT: usize = 100;

/// What a card says of its job as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    /// A turn of the job waits behind earlier work, since `since`.
    Queued { since: i64 },
    /// The job's live turn runs, in its phase when the runtime says.
    Working(Option<ActivityPhase>),
    /// Core no longer lists the job's turn, and how it ended is not yet known.
    Finishing,
    /// A turn answered; the observation core queued to continue the work
    /// has not started.
    Continuing,
    /// The card was recorded before a restart, and nothing since has said
    /// what became of the job.
    Restarted,
    /// The job's last turn ended at `finished_at`.
    Ended {
        turn_state: TurnState,
        delivery: DeliveryOutcome,
        finished_at: i64,
    },
}

/// What a card shows of one job.
pub(super) struct Shown<'a> {
    pub(super) status: Status,
    /// When the person asked, once known.
    pub(super) requested_at: Option<i64>,
    /// What the job's turns did, oldest first: the final reports of those
    /// that ended, and then what is known of one that did not.
    pub(super) reports: Vec<&'a TurnActivity>,
    /// The snapshot of the job's turn running now: the only part shown as
    /// still running.
    pub(super) live: Option<&'a TurnActivity>,
    /// Reports of some of the job's turns are not known here: what is shown
    /// may not be all of its work.
    pub(super) partial: bool,
}

/// Whether a turn's end earns a retained summary: it stopped before
/// finishing, or its report shows work (calls, published reasoning, or
/// delegated and transferred work). An answer alone needs no card.
pub(crate) fn earns_summary(delivery: &Delivery) -> bool {
    delivery.turn_state == TurnState::Interrupted
        || delivery.activity.as_ref().is_some_and(earns_report)
}

pub(super) fn earns_report(report: &TurnActivity) -> bool {
    report.tool_calls > 0 || !report.notes.is_empty() || report.work.is_some()
}

/// The summary an answer carries when its job has no card: what the turn
/// did, and whether its work goes on.
pub(crate) fn summary(delivery: &Delivery, markers: &ActivityMarkers) -> Embed {
    let status = if delivery.continued_by.is_some() {
        Status::Continuing
    } else {
        Status::Ended {
            turn_state: delivery.turn_state,
            delivery: DeliveryOutcome::Delivered,
            finished_at: delivery.finished_at,
        }
    };
    card(
        &Shown {
            status,
            requested_at: Some(delivery.origin.accepted_at),
            reports: delivery.activity.iter().collect(),
            live: None,
            partial: false,
        },
        markers,
    )
}

/// The card's embed.
pub(super) fn card(shown: &Shown<'_>, markers: &ActivityMarkers) -> Embed {
    let merged = Merged::of(&shown.reports, shown.live);
    let mut description = vec![status_line(shown, markers)];
    if let Some(headline) = shown.live.and_then(|live| live.headline.as_deref()) {
        description.push(format!("> {}", escaped(&cut(headline, HEADLINE_LIMIT))));
    }
    if merged.partial || shown.partial {
        description.push(marked(
            &markers.uncertain,
            "Some of this work may not be shown here.",
        ));
    }
    let mut fields = Vec::new();
    let mut field = |name: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            fields.push(EmbedField {
                inline: false,
                name: name.to_owned(),
                value: fitted(&lines, FIELD_VALUE_LENGTH),
            });
        }
    };
    field(
        "Reasoning",
        merged.notes.iter().map(|note| escaped(note)).collect(),
    );
    field("Calls", call_lines(&merged, shown.live, markers));
    field("Work", work_lines(&merged, markers));
    // Metadata that states no fact shows no section.
    let agent = merged
        .agent
        .map(|agent| agent_line(agent, markers))
        .filter(|line| !line.is_empty());
    field("Agent", agent.into_iter().collect());
    Embed {
        author: None,
        color: None,
        description: Some(cut(&description.join("\n"), DESCRIPTION_LENGTH)),
        fields,
        footer: None,
        image: None,
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: None,
        timestamp: None,
        title: None,
        url: None,
        video: None,
    }
}

fn status_line(shown: &Shown<'_>, markers: &ActivityMarkers) -> String {
    let started =
        |at: Option<i64>| at.map_or_else(String::new, |at| format!(" · started <t:{at}:R>"));
    match shown.status {
        Status::Queued { since } => marked(
            &markers.queued,
            &format!("Waiting for earlier work to finish · queued <t:{since}:R>"),
        ),
        Status::Working(phase) => {
            let label = match phase {
                Some(ActivityPhase::Starting) => " · Starting",
                Some(ActivityPhase::Queued) => " · Queued behind other work in this conversation",
                Some(ActivityPhase::Model) => " · Waiting for the model",
                Some(ActivityPhase::Tools) | None => "",
                Some(ActivityPhase::Waiting) => " · Waiting",
                Some(ActivityPhase::Retrying) => " · Retrying the model request",
                Some(ActivityPhase::Compacting) => " · Compacting the conversation",
            };
            marked(
                &markers.working,
                &format!("Working{label}{}", started(shown.requested_at)),
            )
        }
        Status::Finishing => marked(&markers.finishing, "Finishing up"),
        Status::Continuing => marked(
            &markers.working,
            &format!(
                "Answered · delegated work goes on{}",
                started(shown.requested_at)
            ),
        ),
        Status::Restarted => marked(
            &markers.uncertain,
            "Carillon restarted while this was in progress; how it ends shows here if Carillon learns it.",
        ),
        Status::Ended {
            turn_state,
            delivery,
            finished_at,
        } => match (turn_state, delivery) {
            (TurnState::Completed, DeliveryOutcome::Delivered) => {
                let took = shown
                    .requested_at
                    .and_then(|requested| u64::try_from(finished_at - requested).ok())
                    .map_or_else(String::new, |seconds| {
                        format!(" · answered {} after the request", compact(seconds * 1000))
                    });
                marked(&markers.succeeded, &format!("Done{took}"))
            }
            (TurnState::Completed, DeliveryOutcome::Uncertain) => marked(
                &markers.uncertain,
                "Finished, but the reply may not have been delivered.",
            ),
            (_, DeliveryOutcome::Delivered) => {
                marked(&markers.stopped, "Stopped before finishing.")
            }
            (_, DeliveryOutcome::Uncertain) => marked(
                &markers.stopped,
                "Stopped before finishing; the notice about it may not have been delivered.",
            ),
        },
    }
}

/// What the job's turns did together. Calls are counted across them; the
/// latest finished calls, notes and agent are shown; work is one row per
/// native identity, as the latest report of it says. A report's count of
/// work it left out is exact for that report alone: when several reports
/// have work and any left some out, that work may repeat or show in another
/// report, so it is said to be not fully observed rather than counted.
struct Merged<'a> {
    calls: u64,
    recent: Vec<&'a FinishedTool>,
    work: Vec<&'a WorkItem>,
    work_partial: bool,
    work_omitted: u64,
    notes: Vec<&'a str>,
    agent: Option<&'a AgentInfo>,
    partial: bool,
}

impl<'a> Merged<'a> {
    fn of(reports: &[&'a TurnActivity], live: Option<&'a TurnActivity>) -> Self {
        let all = reports.iter().copied().chain(live);
        let mut merged = Merged {
            calls: 0,
            recent: Vec::new(),
            work: Vec::new(),
            work_partial: false,
            work_omitted: 0,
            notes: Vec::new(),
            agent: None,
            partial: false,
        };
        let mut positions = HashMap::new();
        let (mut summaries, mut omitting) = (0_u32, false);
        for report in all {
            merged.calls += u64::from(report.tool_calls);
            merged.recent.extend(&report.recent);
            merged.notes.extend(report.notes.iter().map(String::as_str));
            merged.agent = report.agent.as_ref().or(merged.agent);
            merged.partial |= report.partial;
            if let Some(work) = &report.work {
                summaries += 1;
                merged.work_partial |= work.partial;
                merged.work_omitted = work.omitted.map_or(0, |omitted| u64::from(omitted.get()));
                omitting |= merged.work_omitted > 0;
                for item in &work.items {
                    if let Some(&index) = positions.get(item.id.as_str()) {
                        merged.work[index] = item;
                    } else {
                        positions.insert(item.id.as_str(), merged.work.len());
                        merged.work.push(item);
                    }
                }
            }
        }
        if summaries > 1 && omitting {
            merged.work_omitted = 0;
            merged.work_partial = true;
        }
        // Live work first, then work whose state is unknown, then finished.
        merged.work.sort_by_key(|item| match item.state {
            WorkState::Queued | WorkState::Running | WorkState::Waiting => 0,
            WorkState::Unknown => 1,
            WorkState::Succeeded | WorkState::Failed | WorkState::Cancelled => 2,
        });
        merged
    }
}

/// Finished calls, oldest first, then the live turn's running calls; calls
/// no row shows are counted on a first line.
fn call_lines(
    merged: &Merged<'_>,
    snapshot: Option<&TurnActivity>,
    markers: &ActivityMarkers,
) -> Vec<String> {
    let recent = &merged.recent[merged.recent.len().saturating_sub(RECENT_LINES)..];
    let running = snapshot.map_or(&[][..], |snapshot| &snapshot.running[..]);
    // Running calls beyond their rows are counted on a line of their own.
    let shown = recent.len() + running.len();
    let mut lines = Vec::new();
    let earlier = usize::try_from(merged.calls)
        .unwrap_or(usize::MAX)
        .saturating_sub(shown);
    if earlier > 0 {
        lines.push(format!(
            "… {earlier} earlier call{}",
            if earlier == 1 { "" } else { "s" }
        ));
    }
    for call in recent {
        let mut line = tool_line(
            if call.ok {
                &markers.succeeded
            } else {
                &markers.failed
            },
            &call.tool,
            call.detail.as_deref(),
        );
        if let Some(duration) = call.duration_ms {
            let _ = write!(line, " · {}", compact(duration));
        }
        lines.push(line);
    }
    for call in running.iter().take(RUNNING_LINES) {
        let line = tool_line(&markers.running, &call.tool, call.detail.as_deref());
        lines.push(format!(
            "{line} · {}",
            since(call.started_at, call.estimated)
        ));
    }
    let more = running.len().saturating_sub(RUNNING_LINES);
    if more > 0 {
        lines.push(marked(
            &markers.running,
            &format!("… and {more} more running"),
        ));
    }
    lines
}

/// Delegated and transferred work, one bounded row per item. A known
/// parent indents its children; items beyond the shown rows and an exact
/// further count become "… and N more". Incomplete observation is said as
/// such beside that count, which covers only what is known, and never
/// becomes a guessed total.
fn work_lines(merged: &Merged<'_>, markers: &ActivityMarkers) -> Vec<String> {
    let shown = &merged.work[..merged.work.len().min(WORK_LINES)];
    let mut lines = Vec::new();
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
    let more =
        u64::try_from(merged.work.len() - shown.len()).unwrap_or(u64::MAX) + merged.work_omitted;
    if more > 0 {
        lines.push(marked(&markers.queued, &format!("… and {more} more")));
    }
    if merged.work_partial {
        lines.push(marked(
            &markers.uncertain,
            "… additional work not fully observed",
        ));
    }
    lines
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
        text.push_str(&model_label(actor, markers));
    }
    if let Some(duration) = item.duration_ms {
        let _ = write!(text, " · {}", compact(duration));
    } else if item.state == WorkState::Running
        && let Some(started_at) = item.started_at
    {
        let _ = write!(text, " · {}", since(started_at, item.estimated));
    }
    marked(marker, &text)
}

/// The agent's model, effort and context, as its engine last reported them.
fn agent_line(agent: &AgentInfo, markers: &ActivityMarkers) -> String {
    let mut parts = Vec::new();
    if let Some(model) = &agent.model {
        parts.push(model_label(model, markers));
    }
    if let Some(effort) = &agent.effort {
        parts.push(format!("effort {}", plain(&cut(effort, LABEL_LIMIT))));
    }
    if let Some(context) = agent.context {
        parts.push(match context.window {
            Some(window) => format!(
                "context ≈ {} of {} tokens",
                tokens(context.used),
                tokens(window)
            ),
            None => format!("context ≈ {} tokens", tokens(context.used)),
        });
    }
    parts.join(" · ")
}

/// Branding is an operator's explicit mapping, not inferred provider telemetry.
fn model_label(model: &str, markers: &ActivityMarkers) -> String {
    let marker = markers
        .models
        .iter()
        .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map_or("", |(_, marker)| marker.as_str());
    marked(marker, &plain(&cut(model, LABEL_LIMIT)))
}

/// A native relative start, or an estimated one said as such.
fn since(started_at: i64, estimated: bool) -> String {
    if estimated {
        format!("about <t:{started_at}:R>")
    } else {
        format!("<t:{started_at}:R>")
    }
}

/// A fixed compact duration: `<1s`, `45s`, `2m 15s`, `1h 3m 8s`.
pub(super) fn compact(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    if seconds == 0 {
        return "<1s".to_owned();
    }
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m {seconds}s"),
        _ => format!("{hours}h {minutes}m {seconds}s"),
    }
}

fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{}.{}M", count / 1_000_000, count / 100_000 % 10),
    }
}

/// Lines within `limit` characters, newest kept: older lines give way to
/// one saying how many went.
fn fitted(lines: &[String], limit: usize) -> String {
    let joined = lines.join("\n");
    if joined.chars().count() <= limit {
        return joined;
    }
    let mut kept = Vec::new();
    let mut length = 0;
    for line in lines.iter().rev() {
        let line = cut(line, limit / 2);
        let added = line.chars().count() + 1;
        if length + added > limit - 20 {
            break;
        }
        length += added;
        kept.push(line);
    }
    let left = lines.len() - kept.len();
    kept.push(format!("… {left} more above"));
    kept.reverse();
    kept.join("\n")
}

/// Text a model wrote, shown as written but unable to format, mention or
/// link anything.
fn escaped(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\*_~`|<>[]()#:@".contains(character) {
            shown.push('\\');
        }
        shown.push(character);
    }
    shown
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
