use super::{active_text, final_text, finishing_text};
use crate::config::ActivityMarkers;
use carillon_protocol::model::{
    ActiveTurn, ActivityPhase, DeliveryOutcome, FinishedTool, RunningTool, TurnActivity, TurnState,
    WorkItem, WorkKind, WorkState, WorkSummary,
};
use std::num::NonZeroU32;

fn turn(activity: Option<TurnActivity>) -> ActiveTurn {
    ActiveTurn {
        turn_id: "turn".to_owned(),
        state: TurnState::Running,
        target_id: "300".to_owned(),
        accepted_at: 1_790_000_000,
        activity,
    }
}

fn tools() -> TurnActivity {
    TurnActivity {
        phase: ActivityPhase::Tools,
        headline: Some("Checking tests".to_owned()),
        running: vec![RunningTool {
            tool: "bash".to_owned(),
            detail: Some("echo `id` > x".to_owned()),
            started_at: 1_790_000_000,
        }],
        recent: vec![
            FinishedTool {
                tool: "read".to_owned(),
                detail: Some("README.md".to_owned()),
                ok: true,
            },
            FinishedTool {
                tool: "mcp<@1>".to_owned(),
                detail: None,
                ok: false,
            },
        ],
        tool_calls: 3,
        work: None,
    }
}

#[test]
fn cards_show_bounded_safe_text() {
    let markers = ActivityMarkers::default();
    let mut busy = tools();
    busy.headline = Some("long commentary ".repeat(20));
    busy.running
        .extend(["a", "b", "c", "d"].map(|detail| RunningTool {
            tool: "read".to_owned(),
            detail: Some(detail.to_owned()),
            started_at: 1_790_000_000,
        }));
    busy.tool_calls = 12;
    let text = active_text(&turn(Some(busy.clone())), None, &markers);
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "⏳ Working · started <t:1790000000:R>");
    assert_eq!(lines[1].chars().count(), 102);
    assert_eq!(lines[2], "… 5 earlier calls");
    assert_eq!(lines[3], "✓ read `README.md`");
    assert_eq!(lines[4], "✗ mcp??1?");
    assert_eq!(lines[5], "↳ bash `echo ˋidˋ > x` · <t:1790000000:R>");
    assert_eq!(lines[8], "↳ … and 2 more running");
    assert_eq!(lines.len(), 9);

    // The last snapshot is history only whenever the current snapshot is absent.
    let history = "\n… 10 earlier calls\n✓ read `README.md`\n✗ mcp??1?";
    assert_eq!(
        active_text(&turn(None), Some(&busy), &markers),
        format!("⏳ Working · started <t:1790000000:R>{history}")
    );
    assert_eq!(
        finishing_text(Some(&busy), &markers),
        format!("⏳ Finishing up{history}")
    );
    for (state, delivery, header) in [
        (
            TurnState::Completed,
            DeliveryOutcome::Uncertain,
            "⚠️ Finished, but the reply may not have been delivered.",
        ),
        (
            TurnState::Interrupted,
            DeliveryOutcome::Delivered,
            "⏹️ Stopped before finishing.",
        ),
        (
            TurnState::Interrupted,
            DeliveryOutcome::Uncertain,
            "⏹️ Stopped before finishing; the notice about it may not have been delivered.",
        ),
    ] {
        assert_eq!(
            final_text(state, delivery, Some(&busy), &markers),
            Some(format!("{header}{history}"))
        );
    }
    assert_eq!(
        final_text(
            TurnState::Completed,
            DeliveryOutcome::Delivered,
            Some(&busy),
            &markers
        ),
        None
    );

    busy.running.clear();
    busy.recent = vec![
        FinishedTool {
            tool: "bash".to_owned(),
            detail: Some("x".repeat(3_000)),
            ok: true,
        };
        5
    ];
    let long = active_text(&turn(Some(busy)), None, &markers);
    assert!(long.chars().count() < 2_000);
    assert!(long.lines().all(|line| line.chars().count() <= 130));
}

#[test]
fn defaults_describe_queued_and_model_phases() {
    let markers = ActivityMarkers::default();
    let mut active = turn(None);
    active.state = TurnState::Queued;
    assert_eq!(
        active_text(&active, Some(&tools()), &markers),
        "⏳ Waiting for earlier work to finish · queued <t:1790000000:R>"
    );
    for (phase, expected) in [
        (
            ActivityPhase::Starting,
            "⏳ Working · Starting · started <t:1790000000:R>",
        ),
        (
            ActivityPhase::Model,
            "⏳ Working · Waiting for the model · started <t:1790000000:R>",
        ),
        (
            ActivityPhase::Tools,
            "⏳ Working · started <t:1790000000:R>",
        ),
        (
            ActivityPhase::Retrying,
            "⏳ Working · Retrying the model request · started <t:1790000000:R>",
        ),
        (
            ActivityPhase::Compacting,
            "⏳ Working · Compacting the conversation · started <t:1790000000:R>",
        ),
    ] {
        let mut activity = tools();
        activity.phase = phase;
        assert_eq!(
            active_text(&turn(Some(activity)), None, &markers)
                .lines()
                .next(),
            Some(expected)
        );
    }
}

#[test]
fn overriding_one_marker_leaves_other_defaults() {
    let markers = ActivityMarkers {
        succeeded: "★".to_owned(),
        ..ActivityMarkers::default()
    };
    assert_eq!(
        active_text(&turn(Some(tools())), None, &markers),
        "⏳ Working · started <t:1790000000:R>\n> Checking tests\n★ read `README.md`\n✗ mcp??1?\n↳ bash `echo ˋidˋ > x` · <t:1790000000:R>"
    );
}

#[test]
fn each_marker_uses_its_own_inline_custom_emote_or_symbol() {
    let markers = ActivityMarkers {
        working: "<a:working:101>".to_owned(),
        queued: "<:queued:102>".to_owned(),
        finishing: "⌛".to_owned(),
        running: "➜".to_owned(),
        succeeded: "<:done:103>".to_owned(),
        failed: "<a:error:104>".to_owned(),
        uncertain: "⚠".to_owned(),
        stopped: "■".to_owned(),
    };
    assert_eq!(
        active_text(&turn(Some(tools())), None, &markers),
        "<a:working:101> Working · started <t:1790000000:R>\n> Checking tests\n<:done:103> read `README.md`\n<a:error:104> mcp??1?\n➜ bash `echo ˋidˋ > x` · <t:1790000000:R>"
    );
    let queued = ActiveTurn {
        state: TurnState::Queued,
        ..turn(None)
    };
    assert_eq!(
        active_text(&queued, None, &markers),
        "<:queued:102> Waiting for earlier work to finish · queued <t:1790000000:R>"
    );
    assert_eq!(finishing_text(None, &markers), "⌛ Finishing up");
    assert_eq!(
        final_text(
            TurnState::Completed,
            DeliveryOutcome::Uncertain,
            None,
            &markers
        )
        .as_deref(),
        Some("⚠ Finished, but the reply may not have been delivered.")
    );
    assert_eq!(
        final_text(
            TurnState::Interrupted,
            DeliveryOutcome::Delivered,
            None,
            &markers
        )
        .as_deref(),
        Some("■ Stopped before finishing.")
    );
    assert_eq!(
        final_text(
            TurnState::Interrupted,
            DeliveryOutcome::Uncertain,
            None,
            &markers
        )
        .as_deref(),
        Some("■ Stopped before finishing; the notice about it may not have been delivered.")
    );
}

#[test]
fn empty_markers_leave_no_separator_or_space() {
    let markers = ActivityMarkers {
        working: String::new(),
        queued: String::new(),
        finishing: String::new(),
        running: String::new(),
        succeeded: String::new(),
        failed: String::new(),
        uncertain: String::new(),
        stopped: String::new(),
    };
    assert_eq!(
        active_text(&turn(Some(tools())), None, &markers),
        "Working · started <t:1790000000:R>\n> Checking tests\nread `README.md`\nmcp??1?\nbash `echo ˋidˋ > x` · <t:1790000000:R>"
    );
    let queued = ActiveTurn {
        state: TurnState::Queued,
        ..turn(None)
    };
    assert_eq!(
        active_text(&queued, None, &markers),
        "Waiting for earlier work to finish · queued <t:1790000000:R>"
    );
    assert_eq!(finishing_text(None, &markers), "Finishing up");
    assert_eq!(
        final_text(
            TurnState::Completed,
            DeliveryOutcome::Uncertain,
            None,
            &markers
        )
        .as_deref(),
        Some("Finished, but the reply may not have been delivered.")
    );
    assert_eq!(
        final_text(
            TurnState::Interrupted,
            DeliveryOutcome::Delivered,
            None,
            &markers
        )
        .as_deref(),
        Some("Stopped before finishing.")
    );
    let mut many = tools();
    many.running = many.running.into_iter().cycle().take(4).collect();
    many.tool_calls = 6;
    assert!(active_text(&turn(Some(many)), None, &markers).ends_with("\n… and 1 more running"));
}

fn work(id: &str, kind: WorkKind, label: &str, state: WorkState) -> WorkItem {
    WorkItem {
        id: id.to_owned(),
        kind,
        label: label.to_owned(),
        state,
        parent: None,
        actor: None,
        started_at: None,
    }
}

#[test]
fn work_items_render_generic_bounded_rows_without_provider_knowledge() {
    let markers = ActivityMarkers::default();
    let mut activity = tools();
    activity.running.clear();
    activity.recent.clear();
    activity.tool_calls = 0;
    activity.phase = ActivityPhase::Waiting;
    let mut child = work(
        "a",
        WorkKind::Delegation,
        "research `x`",
        WorkState::Running,
    );
    child.actor = Some("claude-sonnet-5 <@1>".to_owned());
    child.started_at = Some(1_790_000_100);
    let mut grandchild = work("b", WorkKind::Delegation, "grandchild", WorkState::Waiting);
    grandchild.parent = Some("a".to_owned());
    // A parent that is not listed does not indent.
    let mut orphan = work(
        "c",
        WorkKind::Handoff,
        "context handoff to codex",
        WorkState::Unknown,
    );
    orphan.parent = Some("gone".to_owned());
    activity.work = Some(WorkSummary {
        items: vec![
            child,
            grandchild,
            orphan,
            work(
                "d",
                WorkKind::Fork,
                "fork to another thread",
                WorkState::Succeeded,
            ),
            work(
                "e",
                WorkKind::Task,
                "report to this conversation",
                WorkState::Queued,
            ),
        ],
        partial: false,
        omitted: NonZeroU32::new(2),
    });
    let text = active_text(&turn(Some(activity.clone())), None, &markers);
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "⏳ Working · Waiting · started <t:1790000000:R>");
    assert_eq!(
        lines[2],
        "↳ delegation `research ˋxˋ` · running · claude-sonnet-5???1? · <t:1790000100:R>"
    );
    assert_eq!(lines[3], "\u{2003}⏳ delegation `grandchild` · waiting");
    assert_eq!(
        lines[4],
        "⚠️ handoff `context handoff to codex` · state unknown"
    );
    assert_eq!(lines[5], "✓ fork `fork to another thread` · done");
    // One hidden retained item and the exact further count.
    assert_eq!(lines[6], "⏳ … and 3 more");
    assert_eq!(lines.len(), 7);

    // Incomplete observation without a count is said, not counted.
    activity.work = Some(WorkSummary {
        items: vec![work("a", WorkKind::Delegation, "child", WorkState::Failed)],
        partial: true,
        omitted: None,
    });
    let text = active_text(&turn(Some(activity.clone())), None, &markers);
    assert!(
        text.ends_with("✗ delegation `child` · failed\n⚠️ … additional work not fully observed")
    );

    // Hidden retained rows are counted, and do not stand in for coverage the
    // runtime could not confirm (the coordinator's reproduction: five
    // retained rows, partial, no exact further count).
    let children = (0..5)
        .map(|index| {
            work(
                &index.to_string(),
                WorkKind::Delegation,
                &format!("child {index}"),
                WorkState::Running,
            )
        })
        .collect::<Vec<_>>();
    for omitted in [None, NonZeroU32::new(2)] {
        activity.work = Some(WorkSummary {
            items: children.clone(),
            partial: true,
            omitted,
        });
        let text = active_text(&turn(Some(activity.clone())), None, &markers);
        let more = 1 + omitted.map_or(0, NonZeroU32::get);
        assert!(
            text.ends_with(&format!(
                "child 3` · running\n⏳ … and {more} more\n⚠️ … additional work not fully observed"
            )),
            "{text}"
        );
    }

    // Work belongs to the live card: finishing and final cards keep only history.
    assert!(!finishing_text(Some(&activity), &markers).contains("delegation"));
    activity.phase = ActivityPhase::Queued;
    activity.work = None;
    assert_eq!(
        active_text(&turn(Some(activity)), None, &markers),
        "⏳ Working · Queued behind other work in this conversation · started <t:1790000000:R>\n> Checking tests"
    );
}
