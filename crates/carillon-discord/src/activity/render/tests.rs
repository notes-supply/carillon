use super::{Shown, Status, card, compact, earns_summary, summary};
use crate::config::ActivityMarkers;
use carillon_protocol::model::{
    ActivityPhase, AgentInfo, ContextUse, Delivery, DeliveryOutcome, FinishedTool, Origin,
    RunningTool, TurnActivity, TurnState, WorkItem, WorkKind, WorkState, WorkSummary,
};
use std::num::NonZeroU32;
use twilight_model::channel::message::Embed;

fn running(tool: &str, detail: &str) -> RunningTool {
    RunningTool {
        tool: tool.to_owned(),
        detail: Some(detail.to_owned()),
        started_at: 1_790_000_000,
        estimated: false,
    }
}

fn finished(tool: &str, detail: Option<&str>, ok: bool) -> FinishedTool {
    FinishedTool {
        tool: tool.to_owned(),
        detail: detail.map(str::to_owned),
        ok,
        duration_ms: None,
    }
}

fn tools() -> TurnActivity {
    TurnActivity {
        phase: ActivityPhase::Tools,
        headline: Some("Checking tests".to_owned()),
        running: vec![running("bash", "echo `id` > x")],
        recent: vec![
            finished("read", Some("README.md"), true),
            finished("mcp<@1>", None, false),
        ],
        tool_calls: 3,
        work: None,
        notes: Vec::new(),
        agent: None,
        partial: false,
    }
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
        estimated: false,
        duration_ms: None,
    }
}

/// A live card of a job asked for at 1 790 000 000.
fn live<'a>(activity: &'a TurnActivity, reports: Vec<&'a TurnActivity>) -> Shown<'a> {
    Shown {
        status: Status::Working(Some(activity.phase)),
        requested_at: Some(1_790_000_000),
        reports,
        live: Some(activity),
        dropped: false,
    }
}

/// The delivery of a message turn asked for at 1 790 000 000 and finished
/// 45 seconds later, reporting `activity`.
fn delivered(turn_state: TurnState, activity: Option<TurnActivity>) -> Delivery {
    Delivery {
        turn_state,
        id: 1,
        turn_id: "turn".to_owned(),
        target_id: "300".to_owned(),
        feedback_target_id: "300:400".to_owned(),
        content: "answer".to_owned(),
        origin: Origin {
            turn_id: "turn".to_owned(),
            accepted_at: 1_790_000_000,
        },
        finished_at: 1_790_000_045,
        activity,
        continued_by: None,
    }
}

/// The card as text: its description, then each field under its name.
fn text(embed: &Embed) -> String {
    let mut parts = vec![embed.description.clone().unwrap_or_default()];
    for field in &embed.fields {
        parts.push(format!("{}:\n{}", field.name, field.value));
    }
    parts.join("\n")
}

fn field<'a>(embed: &'a Embed, name: &str) -> Option<&'a str> {
    embed
        .fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| field.value.as_str())
}

fn valid(embed: &Embed) -> &Embed {
    assert_eq!(
        twilight_validate::embed::embed(embed)
            .err()
            .map(|error| error.to_string()),
        None
    );
    embed
}

#[test]
fn cards_show_bounded_safe_text() {
    let markers = ActivityMarkers::default();
    let mut busy = tools();
    busy.headline = Some("long commentary ".repeat(20));
    busy.running
        .extend(["a", "b", "c", "d"].map(|detail| running("read", detail)));
    busy.tool_calls = 12;
    let embed = card(&live(&busy, Vec::new()), &markers);
    let description = embed.description.clone().unwrap_or_default();
    let lines = description.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "⏳ Working · started <t:1790000000:R>");
    assert_eq!(lines[1].chars().count(), 102);
    assert_eq!(
        field(&embed, "Calls").map(|calls| calls.lines().collect::<Vec<_>>()),
        Some(vec![
            "… 5 earlier calls",
            "✓ read `README.md`",
            "✗ mcp??1?",
            "↳ bash `echo ˋidˋ > x` · <t:1790000000:R>",
            "↳ read `a` · <t:1790000000:R>",
            "↳ read `b` · <t:1790000000:R>",
            "↳ … and 2 more running",
        ])
    );
    // Headings are plain; nothing but calls is said when nothing else is known.
    assert_eq!(
        embed
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["Calls"]
    );
    valid(&embed);

    // Without a running snapshot, finished calls are history only.
    let history = Shown {
        status: Status::Finishing,
        requested_at: Some(1_790_000_000),
        reports: vec![&busy],
        live: None,
        dropped: false,
    };
    let embed = card(&history, &markers);
    assert_eq!(embed.description.as_deref(), Some("⏳ Finishing up"));
    assert_eq!(
        field(&embed, "Calls"),
        Some("… 10 earlier calls\n✓ read `README.md`\n✗ mcp??1?")
    );

    // Long details and many notes stay within each field's limit.
    busy.running.clear();
    busy.recent = vec![finished("bash", Some(&"x".repeat(3_000)), true); 5];
    busy.notes = vec!["n".repeat(500); 8];
    let embed = card(&live(&busy, Vec::new()), &markers);
    valid(&embed);
    assert!(
        embed
            .fields
            .iter()
            .all(|field| field.value.chars().count() <= 1_024)
    );
    let reasoning = field(&embed, "Reasoning").unwrap_or_default();
    // The latest notes that fit, and how many went.
    assert!(reasoning.starts_with("… 6 more above\n"), "{reasoning}");
    assert_eq!(reasoning.lines().count(), 3);
    assert!(text(&embed).lines().all(|line| line.chars().count() <= 512));
}

#[test]
fn status_rows_say_how_the_job_stands() {
    let markers = ActivityMarkers::default();
    let status = |status| {
        card(
            &Shown {
                status,
                requested_at: Some(1_790_000_000),
                reports: Vec::new(),
                live: None,
                dropped: false,
            },
            &markers,
        )
        .description
        .unwrap_or_default()
    };
    assert_eq!(
        status(Status::Queued {
            since: 1_790_000_005
        }),
        "⏳ Waiting for earlier work to finish · queued <t:1790000005:R>"
    );
    for (phase, expected) in [
        (
            Some(ActivityPhase::Starting),
            "⏳ Working · Starting · started <t:1790000000:R>",
        ),
        (
            Some(ActivityPhase::Model),
            "⏳ Working · Waiting for the model · started <t:1790000000:R>",
        ),
        (
            Some(ActivityPhase::Tools),
            "⏳ Working · started <t:1790000000:R>",
        ),
        (None, "⏳ Working · started <t:1790000000:R>"),
        (
            Some(ActivityPhase::Retrying),
            "⏳ Working · Retrying the model request · started <t:1790000000:R>",
        ),
        (
            Some(ActivityPhase::Compacting),
            "⏳ Working · Compacting the conversation · started <t:1790000000:R>",
        ),
    ] {
        assert_eq!(status(Status::Working(phase)), expected);
    }
    assert_eq!(status(Status::Finishing), "⏳ Finishing up");
    assert_eq!(
        status(Status::Continuing),
        "⏳ Answered · delegated work goes on · started <t:1790000000:R>"
    );
    assert_eq!(
        status(Status::Restarted),
        "⚠️ Carillon restarted while this was in progress; how it ends shows here if Carillon learns it."
    );
    let ended = |turn_state, delivery| Status::Ended {
        turn_state,
        delivery,
        finished_at: 1_790_000_135,
    };
    // A fixed duration, from the request: it includes any wait in the queue.
    assert_eq!(
        status(ended(TurnState::Completed, DeliveryOutcome::Delivered)),
        "✓ Done · answered 2m 15s after the request"
    );
    assert_eq!(
        status(ended(TurnState::Completed, DeliveryOutcome::Uncertain)),
        "⚠️ Finished, but the reply may not have been delivered."
    );
    assert_eq!(
        status(ended(TurnState::Interrupted, DeliveryOutcome::Delivered)),
        "⏹️ Stopped before finishing."
    );
    assert_eq!(
        status(ended(TurnState::Interrupted, DeliveryOutcome::Uncertain)),
        "⏹️ Stopped before finishing; the notice about it may not have been delivered."
    );
}

#[test]
fn durations_are_fixed_and_compact_and_estimated_starts_say_so() {
    for (milliseconds, shown) in [
        (0, "<1s"),
        (999, "<1s"),
        (45_000, "45s"),
        (135_400, "2m 15s"),
        (3_788_000, "1h 3m 8s"),
    ] {
        assert_eq!(compact(milliseconds), shown);
    }
    let markers = ActivityMarkers::default();
    let mut activity = tools();
    activity.headline = None;
    activity.recent = vec![FinishedTool {
        duration_ms: Some(12_300),
        ..finished("bash", Some("cargo test"), true)
    }];
    activity.running = vec![RunningTool {
        estimated: true,
        ..running("bash", "make")
    }];
    activity.tool_calls = 2;
    let mut child = work("a", WorkKind::Delegation, "research", WorkState::Running);
    child.started_at = Some(1_790_000_100);
    child.estimated = true;
    let mut done = work("b", WorkKind::Delegation, "read", WorkState::Succeeded);
    done.started_at = Some(1_790_000_050);
    done.duration_ms = Some(95_000);
    activity.work = Some(WorkSummary {
        items: vec![child, done],
        partial: false,
        omitted: None,
    });
    let embed = card(&live(&activity, Vec::new()), &markers);
    assert_eq!(
        field(&embed, "Calls"),
        Some("✓ bash `cargo test` · 12s\n↳ bash `make` · about <t:1790000000:R>")
    );
    assert_eq!(
        field(&embed, "Work"),
        Some(
            "↳ delegation `research` · running · about <t:1790000100:R>\n✓ delegation `read` · done · 1m 35s"
        )
    );
}

#[test]
fn a_job_shows_its_turns_together_and_the_latest_state_of_each_work_wins() {
    let markers = ActivityMarkers::default();
    // The message's turn answered while its child ran.
    let mut parent = tools();
    parent.running.clear();
    parent.notes = vec!["Delegating the research.".to_owned()];
    parent.agent = Some(AgentInfo {
        model: Some("claude-sonnet-5".to_owned()),
        effort: Some("high".to_owned()),
        context: Some(ContextUse {
            used: 45_000,
            window: Some(200_000),
        }),
    });
    let mut child = work(
        "child",
        WorkKind::Delegation,
        "research",
        WorkState::Running,
    );
    child.actor = Some("claude-haiku-4-5".to_owned());
    child.started_at = Some(1_790_000_010);
    parent.work = Some(WorkSummary {
        items: vec![
            child.clone(),
            work("report", WorkKind::Task, "report back", WorkState::Queued),
        ],
        partial: false,
        omitted: None,
    });
    // Its follow-up saw the child finish, and is reporting it now.
    let mut report = tools();
    report.headline = Some("Summarising the child's findings.".to_owned());
    report.running.clear();
    report.recent = vec![finished("read", Some("findings.md"), true)];
    report.tool_calls = 1;
    report.notes = vec!["The child found two issues.".to_owned()];
    child.state = WorkState::Succeeded;
    child.duration_ms = Some(61_000);
    report.work = Some(WorkSummary {
        items: vec![
            child,
            work("report", WorkKind::Task, "report back", WorkState::Running),
        ],
        partial: false,
        omitted: None,
    });
    let embed = card(&live(&report, vec![&parent]), &markers);
    valid(&embed);
    assert_eq!(
        field(&embed, "Calls"),
        Some("… 1 earlier call\n✓ read `README.md`\n✗ mcp??1?\n✓ read `findings.md`")
    );
    assert_eq!(
        field(&embed, "Work"),
        Some(
            "↳ task `report back` · running\n✓ delegation `research` · done · claude-haiku-4-5 · 1m 1s"
        )
    );
    assert_eq!(
        field(&embed, "Reasoning"),
        Some("Delegating the research.\nThe child found two issues.")
    );
    // The agent stays as last reported when a later report says nothing of it.
    assert_eq!(
        field(&embed, "Agent"),
        Some("claude-sonnet-5 · effort high · context ≈ 45k of 200k tokens")
    );
    assert!(text(&embed).contains("> Summarising the child's findings."));
}

#[test]
fn model_text_cannot_format_mention_or_link() {
    let markers = ActivityMarkers::default();
    let mut activity = tools();
    activity.headline = Some("**bold** <@200> @everyone".to_owned());
    activity.notes =
        vec!["See [here](https://example.invalid) and <#300> `code` # title".to_owned()];
    let embed = card(&live(&activity, Vec::new()), &markers);
    assert!(
        text(&embed).contains(r"> \*\*bold\*\* \<\@200\> \@everyone"),
        "{}",
        text(&embed)
    );
    assert_eq!(
        field(&embed, "Reasoning"),
        Some(r"See \[here\]\(https\://example.invalid\) and \<\#300\> \`code\` \# title")
    );
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
    let mut items = vec![
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
    ];
    items.extend((0..3).map(|index| {
        work(
            &format!("f{index}"),
            WorkKind::Task,
            "more",
            WorkState::Succeeded,
        )
    }));
    activity.work = Some(WorkSummary {
        items,
        partial: false,
        omitted: NonZeroU32::new(2),
    });
    let embed = card(&live(&activity, Vec::new()), &markers);
    assert_eq!(
        embed
            .description
            .as_deref()
            .and_then(|text| text.lines().next()),
        Some("⏳ Working · Waiting · started <t:1790000000:R>")
    );
    assert_eq!(
        field(&embed, "Work").map(|work| work.lines().collect::<Vec<_>>()),
        Some(vec![
            "↳ delegation `research ˋxˋ` · running · claude-sonnet-5???1? · <t:1790000100:R>",
            "\u{2003}⏳ delegation `grandchild` · waiting",
            "⏳ task `report to this conversation` · queued",
            "⚠️ handoff `context handoff to codex` · state unknown",
            "✓ fork `fork to another thread` · done",
            "✓ task `more` · done",
            // Two hidden rows and the exact further count.
            "⏳ … and 4 more",
        ])
    );

    // Incomplete observation without a count is said, not counted.
    activity.work = Some(WorkSummary {
        items: vec![work("a", WorkKind::Delegation, "child", WorkState::Failed)],
        partial: true,
        omitted: None,
    });
    let embed = card(&live(&activity, Vec::new()), &markers);
    assert_eq!(
        field(&embed, "Work"),
        Some("✗ delegation `child` · failed\n⚠️ … additional work not fully observed")
    );
}

#[test]
fn work_a_jobs_reports_left_uncounted_is_said_to_be_unobserved_not_totalled() {
    let markers = ActivityMarkers::default();
    let report = |items: &[(&str, WorkState)], omitted: u32| {
        let mut report = tools();
        report.running.clear();
        report.work = Some(WorkSummary {
            items: items
                .iter()
                .map(|(id, state)| work(id, WorkKind::Delegation, id, *state))
                .collect(),
            partial: false,
            omitted: NonZeroU32::new(omitted),
        });
        report
    };
    let work_of = |earlier: &TurnActivity, latest: &TurnActivity| {
        let embed = card(&live(latest, vec![earlier]), &markers);
        field(&embed, "Work").map(|work| work.lines().map(str::to_owned).collect::<Vec<_>>())
    };
    let unobserved = "⚠️ … additional work not fully observed";
    let (running, done) = (WorkState::Running, WorkState::Succeeded);
    // Work an earlier report left uncounted is not forgotten by a later
    // report that omits nothing, nor counted by it.
    let earlier = report(&[("a", running)], 3);
    assert_eq!(
        work_of(&earlier, &report(&[("b", done)], 0)),
        Some(vec![
            "↳ delegation `a` · running".to_owned(),
            "✓ delegation `b` · done".to_owned(),
            unobserved.to_owned(),
        ])
    );
    // Reports that each omit some work make no sum or maximum of it: the
    // hidden work may be the same, or shown by another report.
    assert_eq!(
        work_of(&report(&[("a", running)], 2), &report(&[("a", done)], 2)),
        Some(vec![
            "✓ delegation `a` · done".to_owned(),
            unobserved.to_owned()
        ])
    );
    let recovered = work_of(
        &report(&[("a", running)], 1),
        &report(&[("a", done), ("b", done)], 0),
    );
    assert_eq!(
        recovered,
        Some(vec![
            "✓ delegation `a` · done".to_owned(),
            "✓ delegation `b` · done".to_owned(),
            unobserved.to_owned(),
        ])
    );
    // Known work beyond the shown rows is still counted exactly.
    let first = [
        ("g0", done),
        ("g1", done),
        ("g2", done),
        ("g3", done),
        ("g4", done),
    ];
    let second = [("h0", done), ("h1", done), ("h2", done)];
    let lines = work_of(&report(&first, 2), &report(&second, 0)).unwrap_or_default();
    assert_eq!(
        lines[6..],
        ["⏳ … and 2 more".to_owned(), unobserved.to_owned()]
    );
    // One report's own count is exact, beside reports with no work at all.
    let mut workless = tools();
    workless.work = None;
    assert_eq!(
        work_of(&workless, &report(&[("a", running)], 5)),
        Some(vec![
            "↳ delegation `a` · running".to_owned(),
            "⏳ … and 5 more".to_owned(),
        ])
    );
}

#[test]
fn an_agent_the_runtime_said_nothing_of_shows_no_agent_section() {
    let markers = ActivityMarkers::default();
    let mut activity = tools();
    activity.work = Some(WorkSummary {
        items: vec![work("a", WorkKind::Delegation, "child", WorkState::Running)],
        partial: false,
        omitted: None,
    });
    let agent_of = |agent: AgentInfo| {
        let mut activity = activity.clone();
        activity.agent = Some(agent);
        let embed = card(&live(&activity, Vec::new()), &markers);
        // Discord's limits, as Twilight checks them, allow either way.
        valid(&embed);
        (field(&embed, "Agent").map(str::to_owned), embed)
    };
    // Valid metadata with no facts in it shows no section; the rest stays.
    let (agent, embed) = agent_of(AgentInfo::default());
    assert!(field(&embed, "Calls").is_some() && field(&embed, "Work").is_some());
    assert_eq!(agent, None);
    // Any fact it has is shown, a context of no tokens included.
    let model = AgentInfo {
        model: Some("claude-sonnet-5".to_owned()),
        ..AgentInfo::default()
    };
    assert_eq!(agent_of(model).0.as_deref(), Some("claude-sonnet-5"));
    let effort = AgentInfo {
        effort: Some("low".to_owned()),
        ..AgentInfo::default()
    };
    assert_eq!(agent_of(effort).0.as_deref(), Some("effort low"));
    let empty_context = AgentInfo {
        context: Some(ContextUse {
            used: 0,
            window: None,
        }),
        ..AgentInfo::default()
    };
    assert_eq!(
        agent_of(empty_context).0.as_deref(),
        Some("context ≈ 0 tokens")
    );
}

#[test]
fn markers_are_configured_per_row_and_empty_ones_leave_no_space() {
    let custom = ActivityMarkers {
        working: "<a:working:101>".to_owned(),
        queued: "<:queued:102>".to_owned(),
        finishing: "⌛".to_owned(),
        running: "➜".to_owned(),
        succeeded: "<:done:103>".to_owned(),
        failed: "<a:error:104>".to_owned(),
        uncertain: "⚠".to_owned(),
        stopped: "■".to_owned(),
    };
    let activity = tools();
    let embed = card(&live(&activity, Vec::new()), &custom);
    assert_eq!(
        text(&embed),
        "<a:working:101> Working · started <t:1790000000:R>\n> Checking tests\nCalls:\n<:done:103> read `README.md`\n<a:error:104> mcp??1?\n➜ bash `echo ˋidˋ > x` · <t:1790000000:R>"
    );
    let empty = ActivityMarkers {
        working: String::new(),
        queued: String::new(),
        finishing: String::new(),
        running: String::new(),
        succeeded: String::new(),
        failed: String::new(),
        uncertain: String::new(),
        stopped: String::new(),
    };
    let embed = card(&live(&activity, Vec::new()), &empty);
    assert_eq!(
        text(&embed),
        "Working · started <t:1790000000:R>\n> Checking tests\nCalls:\nread `README.md`\nmcp??1?\nbash `echo ˋidˋ > x` · <t:1790000000:R>"
    );
}

#[test]
fn a_partial_report_says_so() {
    let mut activity = tools();
    activity.partial = true;
    let embed = card(&live(&activity, Vec::new()), &ActivityMarkers::default());
    assert!(
        embed
            .description
            .unwrap_or_default()
            .ends_with("⚠️ Part of this work's report was left out to keep it short.")
    );
}

/// The final reports of either runtime's shape, from the shared wire
/// fixtures, through an ordinary delivery into the one renderer.
#[test]
fn every_runtimes_report_renders_as_a_summary_within_discords_limits() -> anyhow::Result<()> {
    #[derive(serde::Deserialize)]
    struct Fixtures {
        activities: Vec<TurnActivity>,
    }
    let fixtures: Fixtures = serde_json::from_str(include_str!(
        "../../../../../packages/protocol/fixtures/activity.json"
    ))?;
    let markers = ActivityMarkers::default();
    for activity in fixtures.activities {
        let delivery = delivered(TurnState::Completed, Some(activity.clone()));
        let embed = summary(&delivery, &markers);
        valid(&embed);
        let shown = text(&embed);
        if earns_summary(&delivery) {
            assert!(
                shown.starts_with("✓ Done · answered 45s after the request"),
                "{shown}"
            );
        }
        valid(&card(&live(&activity, Vec::new()), &markers));
    }
    Ok(())
}

#[test]
fn work_or_a_stop_earns_a_summary() {
    let mut answer_only = tools();
    answer_only.running.clear();
    answer_only.recent.clear();
    answer_only.tool_calls = 0;
    let earns = |turn_state, report: Option<&TurnActivity>| {
        earns_summary(&delivered(turn_state, report.cloned()))
    };
    let completed = TurnState::Completed;
    assert!(!earns(completed, None));
    assert!(!earns(completed, Some(&answer_only)));
    let mut thought = answer_only.clone();
    thought.notes = vec!["Thinking.".to_owned()];
    assert!(earns(completed, Some(&thought)));
    let mut called = answer_only.clone();
    called.tool_calls = 1;
    assert!(earns(completed, Some(&called)));
    let mut delegated = answer_only.clone();
    delegated.work = Some(WorkSummary {
        items: vec![work("a", WorkKind::Delegation, "child", WorkState::Running)],
        partial: false,
        omitted: None,
    });
    assert!(earns(completed, Some(&delegated)));
    // A turn that stopped earns one whatever it reported, and it shows only the stop.
    assert!(earns(TurnState::Interrupted, None));
    assert!(earns(TurnState::Interrupted, Some(&answer_only)));
    let stopped = summary(
        &delivered(TurnState::Interrupted, None),
        &ActivityMarkers::default(),
    );
    assert_eq!(text(valid(&stopped)), "⏹️ Stopped before finishing.");
}
