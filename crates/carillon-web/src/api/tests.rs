//! What the page reads: core's wire types as the page's own.

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{Method, StatusCode, header::CONTENT_TYPE},
};
use carillon_protocol::{
    conversations::{Transcript, TranscriptAnswer, TranscriptTurn, TurnSource},
    model::{
        ActivityPhase, AnswerFile, DeliveryState, Job, JobTurn, Origin, TurnActivity,
        TurnAttachment, TurnState,
    },
    turns::{ControlAction, Silence},
};
use serde_json::{Value, json};

use crate::fixture::{Fixture, bearer, conversation};

const CHAT: &str = "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b";

fn turn(id: &str, source: TurnSource, state: TurnState) -> TranscriptTurn {
    TranscriptTurn {
        turn_id: id.to_owned(),
        source,
        state,
        accepted_at: 10,
        finished_at: None,
        report: None,
        answer: None,
    }
}

fn message(event: &str, text: &str) -> TurnSource {
    TurnSource::Message {
        event_id: event.to_owned(),
        text: text.to_owned(),
        command: false,
        files: Vec::new(),
    }
}

fn answer(content: &str, silence: Option<Silence>) -> TranscriptAnswer {
    TranscriptAnswer {
        content: content.to_owned(),
        silence,
        files: Vec::new(),
        delivery: DeliveryState::Delivered,
    }
}

fn ended(mut turn: TranscriptTurn, answer: TranscriptAnswer) -> TranscriptTurn {
    turn.finished_at = Some(20);
    turn.answer = Some(answer);
    turn
}

#[tokio::test]
async fn conversations_are_listed_as_core_orders_them_with_what_a_poll_compares() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    {
        let mut core = fixture.core();
        let mut working = conversation("second", CHAT, 7);
        working.at_work = 2;
        core.conversations.push(working);
        core.conversations
            .push(conversation("assistant", "not-a-uuid", 5));
        core.conversations.push(conversation(
            "assistant",
            "0190f3a8-7b1c-7def-8a2b-000000000001",
            3,
        ));
    }
    let listed = fixture.get("/api/conversations", &token).await?;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(
        listed.json()?,
        json!({
            "conversations": [
                {
                    "id": CHAT, "agent_id": "second", "title": "title 7",
                    "started_at": 100, "active_at": 107, "activity": 7, "at_work": 2,
                },
                {
                    "id": "0190f3a8-7b1c-7def-8a2b-000000000001", "agent_id": "assistant",
                    "title": "title 3", "started_at": 100, "active_at": 103, "activity": 3,
                    "at_work": 0,
                },
            ],
            "next": null,
        })
    );
    let below = fixture.get("/api/conversations?before=7", &token).await?;
    assert_eq!(
        below.json()?["conversations"].as_array().map(Vec::len),
        Some(1)
    );
    assert!(
        fixture
            .core()
            .asked
            .contains(&"/v1/conversations?before=7".to_owned())
    );
    Ok(())
}

fn report() -> TurnActivity {
    TurnActivity {
        phase: ActivityPhase::Model,
        headline: Some("done".to_owned()),
        running: Vec::new(),
        recent: Vec::new(),
        tool_calls: 2,
        work: None,
        notes: Vec::new(),
        agent: None,
        partial: false,
        controls: Vec::new(),
    }
}

/// One turn of each kind a person can see, the first ended with `report`.
fn transcript(report: &TurnActivity) -> Vec<TranscriptTurn> {
    let mut answered = ended(
        turn(
            "t1",
            TurnSource::Message {
                event_id: "m-1".to_owned(),
                text: "<b>look</b> at *this*".to_owned(),
                command: true,
                files: vec![TurnAttachment {
                    path: "capture-a/1".to_owned(),
                    name: "report.pdf".to_owned(),
                    size: 8,
                    content_type: Some("application/pdf".to_owned()),
                    sha256: "ab".repeat(32),
                }],
            },
            TurnState::Completed,
        ),
        answer("Here is the **chart**. <script>x</script>", None),
    );
    answered.report = Some(report.clone());
    if let Some(answer) = &mut answered.answer {
        answer.files.push(AnswerFile {
            path: "capture-a/0".to_owned(),
            name: "chart.png".to_owned(),
            source: "/home/owner/work/secret-project/chart.png".to_owned(),
            size: 25,
            sha256: "cd".repeat(32),
        });
    }
    vec![
        answered,
        ended(
            turn("t2", message("m-2", "and then?"), TurnState::Interrupted),
            answer("The runtime ran out of *credit*.", None),
        ),
        ended(
            turn("t3", message("m-3", "also this"), TurnState::Completed),
            answer("", Some(Silence::Steered)),
        ),
        ended(
            turn("t4", message("m-4", "never mind"), TurnState::Completed),
            answer("", Some(Silence::Stopped)),
        ),
        ended(
            turn(
                "t5",
                TurnSource::Opened {
                    label: Some("Scheduled run: morning summary".to_owned()),
                },
                TurnState::Completed,
            ),
            answer("Good morning.", None),
        ),
        turn("t6", TurnSource::Opened { label: None }, TurnState::Running),
        turn("t7", message("m-7", "waiting"), TurnState::Queued),
    ]
}

/// Nothing in `wire` says where a file is on this host, or how core keeps it.
fn keeps_back(wire: &str, installation: &std::path::Path) -> Result<()> {
    for kept_back in [
        "secret-project",
        "/home/",
        "source",
        "sha256",
        "\"path\"",
        "delivery",
        installation.to_str().context("path")?,
    ] {
        assert!(!wire.contains(kept_back), "{kept_back} reached the page");
    }
    Ok(())
}

#[tokio::test]
async fn a_turn_reads_as_who_began_it_and_how_it_stands_in_plain_terms() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let store = fixture.directory.path().join("state/attachments");
    std::fs::create_dir_all(store.join("capture-a"))?;
    std::fs::write(
        store.join("capture-a/0"),
        b"\x89PNG\r\n\x1a\nrest of a picture",
    )?;
    std::fs::write(store.join("capture-a/1"), b"%PDF-1.7")?;
    let report = report();
    let turns = transcript(&report);
    {
        let mut core = fixture.core();
        core.conversations.push(conversation("second", CHAT, 9));
        core.transcripts.insert(
            format!("second/{CHAT}"),
            Transcript {
                turns,
                earlier: true,
            },
        );
    }
    let read = fixture
        .get(&format!("/api/conversations/{CHAT}/turns"), &token)
        .await?;
    assert_eq!(read.status, StatusCode::OK, "{}", read.text());
    let body = read.json()?;
    assert_eq!(
        body["before"], "t1",
        "older turns are read before the first"
    );
    let turns = body["turns"].as_array().context("no turns")?;
    let link = |file: &Value| file["url"].as_str().map(str::to_owned).unwrap_or_default();

    let first = &turns[0];
    assert_eq!(first["id"], "t1");
    assert_eq!(
        (&first["accepted_at"], &first["finished_at"]),
        (&json!(10), &json!(20))
    );
    assert_eq!(first["from"]["kind"], "person");
    assert_eq!(first["from"]["event_id"], "m-1");
    assert_eq!(first["from"]["text"], "<b>look</b> at *this*", "as written");
    assert_eq!(first["from"]["command"], true);
    let sent = &first["from"]["files"][0];
    assert_eq!(
        (&sent["name"], &sent["size"], &sent["type"]),
        (
            &json!("report.pdf"),
            &json!(8),
            &json!("application/octet-stream")
        )
    );
    assert!(link(sent).starts_with("/files/capture-a/1?name=report.pdf&exp="));
    assert_eq!(first["state"], "answered");
    assert_eq!(
        first["html"],
        "<p>Here is the <strong>chart</strong>. &lt;script&gt;x&lt;/script&gt;</p>\n"
    );
    let chart = &first["files"][0];
    assert_eq!(
        (&chart["name"], &chart["size"], &chart["type"]),
        (&json!("chart.png"), &json!(25), &json!("image/png"))
    );
    assert!(link(chart).starts_with("/files/capture-a/0?name=chart.png&exp="));
    assert_eq!(first["report"], serde_json::to_value(&report)?);

    assert_eq!(turns[1]["state"], "interrupted");
    assert_eq!(
        turns[1]["html"],
        "<p>The runtime ran out of <em>credit</em>.</p>\n"
    );
    assert_eq!(turns[2]["state"], "steered");
    assert_eq!(turns[3]["state"], "stopped");
    for silent in [&turns[2], &turns[3]] {
        assert!(silent.get("html").is_none() && silent.get("files").is_none());
    }
    assert_eq!(
        turns[4]["from"],
        json!({"kind": "runtime", "label": "Scheduled run: morning summary"})
    );
    assert_eq!(turns[4]["state"], "answered");
    assert_eq!(turns[5]["from"], json!({"kind": "runtime", "label": null}));
    assert_eq!(turns[5]["state"], "running");
    assert_eq!(turns[5]["finished_at"], Value::Null);
    assert_eq!(turns[6]["state"], "queued");

    keeps_back(&read.text(), fixture.directory.path())?;
    let earlier = format!("/api/conversations/{CHAT}/turns?before=t1");
    assert_eq!(fixture.get(&earlier, &token).await?.status, StatusCode::OK);
    assert!(
        fixture
            .core()
            .asked
            .contains(&format!("/v1/conversations/second/{CHAT}?before=t1"))
    );
    Ok(())
}

#[tokio::test]
async fn a_conversation_is_named_by_one_lowercase_uuid_that_core_knows() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    fixture
        .core()
        .conversations
        .push(conversation("assistant", CHAT, 1));
    let turns = |id: &str| format!("/api/conversations/{id}/turns");
    assert_eq!(
        fixture.get(&turns(CHAT), &token).await?.status,
        StatusCode::OK
    );
    for malformed in [
        "0190F3A8-7B1C-7DEF-8A2B-3C4D5E6F7A8B",
        "0190f3a87b1c7def8a2b3c4d5e6f7a8b",
        "%7B0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b%7D",
        "urn:uuid:0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b",
        "web-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b",
        "assistant",
    ] {
        let answer = fixture.get(&turns(malformed), &token).await?;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{malformed}");
    }
    assert!(
        fixture
            .core()
            .asked
            .iter()
            .all(|asked| asked.contains(CHAT)),
        "a malformed name reached core"
    );
    let unknown = turns("0190f3a8-7b1c-7def-8a2b-ffffffffffff");
    let answer = fixture.get(&unknown, &token).await?;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);

    // One audience under two agents is nothing the page made.
    fixture
        .core()
        .conversations
        .push(conversation("second", CHAT, 2));
    let answer = fixture.get(&turns(CHAT), &token).await?;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    Ok(())
}

fn job(
    turn: &str,
    feedback: Option<&str>,
    state: TurnState,
    activity: Option<TurnActivity>,
) -> Job {
    Job {
        origin: Origin {
            turn_id: turn.to_owned(),
            accepted_at: 10,
        },
        target_id: CHAT.to_owned(),
        feedback_target_id: feedback.map(str::to_owned),
        turns: vec![JobTurn {
            turn_id: turn.to_owned(),
            state,
            accepted_at: 10,
            finished_at: None,
            activity,
            delivery: None,
        }],
        earlier: false,
    }
}

#[tokio::test]
async fn a_live_read_says_how_each_named_turn_stands_and_what_may_be_done_with_it() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let offering = |phase, controls: &[ControlAction]| TurnActivity {
        phase,
        controls: controls.to_vec(),
        ..report()
    };
    let waiting = offering(
        ActivityPhase::Queued,
        &[ControlAction::Steer, ControlAction::Stop],
    );
    fixture.core().jobs = vec![
        job("queued", Some("m-1"), TurnState::Queued, None),
        job(
            "held",
            Some("m-2"),
            TurnState::Running,
            Some(waiting.clone()),
        ),
        job(
            "working",
            Some("m-3"),
            TurnState::Running,
            Some(offering(ActivityPhase::Tools, &[])),
        ),
        job(
            "opened",
            None,
            TurnState::Running,
            Some(offering(ActivityPhase::Model, &[ControlAction::Stop])),
        ),
        job("done", Some("m-5"), TurnState::Completed, Some(report())),
        job("failed", Some("m-6"), TurnState::Interrupted, None),
        job("unasked", Some("m-7"), TurnState::Running, None),
    ];
    let read = fixture
        .get(
            "/api/turns?ids=queued,held,working,opened,done,failed,unknown",
            &token,
        )
        .await?;
    assert_eq!(read.status, StatusCode::OK, "{}", read.text());
    let body = read.json()?;
    let stands: Vec<(&str, &str, Vec<&str>)> = body["turns"]
        .as_array()
        .context("no turns")?
        .iter()
        .map(|turn| {
            (
                turn["id"].as_str().unwrap_or_default(),
                turn["state"].as_str().unwrap_or_default(),
                turn["activity"]["controls"]
                    .as_array()
                    .map(|controls| controls.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        stands,
        [
            ("queued", "queued", vec![]),
            // The runtime holds it behind another turn: still waiting, to a person.
            ("held", "queued", vec!["steer", "stop"]),
            ("working", "running", vec![]),
            // No message began it, so no control could name it.
            ("opened", "running", vec![]),
            ("done", "ended", vec![]),
            ("failed", "ended", vec![]),
        ]
    );
    let mut held = serde_json::to_value(&waiting)?;
    assert_eq!(body["turns"][1]["activity"], held.take());
    assert_eq!(body["turns"][0]["activity"], Value::Null);
    assert!(!read.text().contains("target_id") && !read.text().contains("m-2"));

    let none = fixture.get("/api/turns?ids=", &token).await?;
    assert_eq!(none.json()?, json!({"turns": []}));
    let many = vec!["t"; 33].join(",");
    let refused = fixture
        .get(&format!("/api/turns?ids={many}"), &token)
        .await?;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        fixture
            .core()
            .asked
            .iter()
            .filter(|asked| asked.starts_with("/v1/jobs"))
            .count(),
        1,
        "only the read that named turns asked core"
    );
    Ok(())
}

#[tokio::test]
async fn a_control_reaches_core_for_the_message_it_names_and_a_refusal_comes_back() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let event = "m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa";
    let control = |event: &str, body: &str| {
        bearer(
            Method::POST,
            &format!("/api/messages/{event}/control"),
            &token,
        )
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
    };
    for action in ["steer", "interrupt", "stop"] {
        let answer = fixture
            .send(control(event, &json!({ "action": action }).to_string())?)
            .await?;
        assert_eq!(answer.status, StatusCode::NO_CONTENT, "{action}");
        assert!(answer.body.is_empty());
    }
    let relayed: Vec<(String, ControlAction)> = fixture
        .core()
        .controls
        .iter()
        .map(|(event, control)| (event.clone(), control.action))
        .collect();
    assert_eq!(
        relayed,
        [
            (event.to_owned(), ControlAction::Steer),
            (event.to_owned(), ControlAction::Interrupt),
            (event.to_owned(), ControlAction::Stop),
        ]
    );
    for (event, body) in [
        (event, "{\"action\":\"resume\"}"),
        (event, "{}"),
        ("runtime:schedule-1", "{\"action\":\"stop\"}"),
        (
            "0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
            "{\"action\":\"stop\"}",
        ),
    ] {
        let answer = fixture.send(control(event, body)?).await?;
        assert!(answer.status.is_client_error(), "{event} {body}");
    }
    assert_eq!(
        fixture.core().controls.len(),
        3,
        "a malformed control reached core"
    );

    fixture.core().refusal = Some((StatusCode::CONFLICT, "That turn no longer waits."));
    let late = fixture
        .send(control(event, "{\"action\":\"stop\"}")?)
        .await?;
    assert_eq!(late.status, StatusCode::CONFLICT);
    assert_eq!(late.json()?, json!({"error": "That turn no longer waits."}));
    Ok(())
}

#[tokio::test]
async fn commands_are_those_of_the_conversations_own_agent() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let commands = |query: &str| format!("/api/conversations/{CHAT}/commands{query}");
    let new = fixture.get(&commands("?agent=assistant"), &token).await?;
    assert_eq!(new.status, StatusCode::OK, "{}", new.text());
    assert_eq!(
        new.json()?,
        json!({"commands": [{
            "name": "skill:assistant",
            "description": format!("in {CHAT}"),
            "hint": CHAT,
        }]})
    );
    let unnamed = fixture.get(&commands(""), &token).await?;
    assert_eq!(unnamed.status, StatusCode::NOT_FOUND);

    fixture
        .core()
        .conversations
        .push(conversation("second", CHAT, 1));
    let own = fixture.get(&commands(""), &token).await?;
    assert_eq!(own.json()?["commands"][0]["name"], "skill:second");
    let other = fixture.get(&commands("?agent=assistant"), &token).await?;
    assert_eq!(other.status, StatusCode::CONFLICT);

    fixture.core().refusal = Some((
        StatusCode::SERVICE_UNAVAILABLE,
        "The agent's runtime is disabled.",
    ));
    let unavailable = fixture.get(&commands(""), &token).await?;
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        unavailable.json()?,
        json!({"error": "The agent's runtime is disabled."})
    );
    Ok(())
}
