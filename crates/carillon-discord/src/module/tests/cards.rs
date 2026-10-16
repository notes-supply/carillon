use super::*;

/// A job at work, whose origin turn is `job`.
fn working_card(headline: &str) -> CardRequest {
    CardRequest {
        job: carillon_protocol::model::Job {
            origin: carillon_protocol::model::Origin {
                turn_id: "job".to_owned(),
                accepted_at: 1_790_000_000,
            },
            target_id: "300".to_owned(),
            turns: vec![carillon_protocol::model::JobTurn {
                turn_id: "job".to_owned(),
                state: carillon_protocol::model::TurnState::Running,
                accepted_at: 1_790_000_000,
                finished_at: None,
                activity: Some(carillon_protocol::model::TurnActivity {
                    phase: carillon_protocol::model::ActivityPhase::Tools,
                    headline: Some(headline.to_owned()),
                    running: Vec::new(),
                    recent: Vec::new(),
                    tool_calls: 0,
                    work: None,
                    notes: Vec::new(),
                    agent: None,
                    partial: false,
                }),
                delivery: None,
            }],
            earlier: false,
        },
        seen: None,
        markers: ActivityMarkers::default(),
    }
}

/// The same job, settled: answered and its answer delivered.
fn settled_card() -> CardRequest {
    let mut request = working_card("done");
    let turn = &mut request.job.turns[0];
    turn.state = carillon_protocol::model::TurnState::Completed;
    turn.finished_at = Some(1_790_000_045);
    turn.delivery = Some(carillon_protocol::model::DeliveryState::Delivered);
    request
}

/// What `module` makes of `request`, and how long it took.
async fn drawn(
    module: &[u8],
    request: &CardRequest,
) -> anyhow::Result<(
    anyhow::Result<Option<twilight_model::channel::message::Embed>>,
    Duration,
)> {
    let host = super::super::Host::new()?;
    let module = host.load(module)?;
    let started = std::time::Instant::now();
    let card = super::super::card(&host, &module, request).await;
    Ok((card, started.elapsed()))
}

#[tokio::test]
async fn a_card_the_module_cannot_give_is_a_bounded_failure() -> anyhow::Result<()> {
    let request = working_card("Working");
    let answer = |text: &str| {
        format!(
            "(global.set $output (i32.const 16384)) (i32.const {})",
            text.len()
        )
    };
    let embed = |embed: Value| json!({"Card": {"Show": embed}}).to_string();
    let failing = [
        (
            "not JSON",
            "not json".to_owned(),
            "decode Discord module outcome: ",
        ),
        (
            "another answer",
            replying("hello", &[]),
            "Discord module answered a card with something else",
        ),
        (
            "a failure",
            r#"{"Failure":"no card"}"#.to_owned(),
            "Discord module failed: no card",
        ),
        (
            "not an embed",
            embed(json!({"description": "no type"})),
            "the card's embed is unusable: not Discord embeds: ",
        ),
        (
            "over Discord's limits",
            embed(json!({"type": "rich", "description": "x".repeat(4_097)})),
            "the card's embed is unusable: beyond Discord's limits: ",
        ),
        (
            "a removal while the job works",
            r#"{"Card":"Remove"}"#.to_owned(),
            "Discord module removed the card of a job that has not settled",
        ),
    ];
    for (case, output, refused) in failing {
        let module = other_cards(&answer(&output), &[(16384, &output)])?;
        let (card, _) = drawn(&module, &request).await?;
        let failure = card.err().with_context(|| format!("{case} was shown"))?;
        assert!(
            format!("{failure:#}").starts_with(refused),
            "{case}: {failure:#}"
        );
    }
    // The same removal of a settled job is the module's to decide.
    let remove = r#"{"Card":"Remove"}"#;
    let module = other_cards(&answer(remove), &[(16384, remove)])?;
    assert_eq!(drawn(&module, &settled_card()).await?.0?, None);

    for (case, on_card, refused) in [
        ("a trap", "(unreachable)", "guest failed: "),
        (
            "a guest that never ends",
            "(loop $forever (br $forever)) (i32.const 0)",
            "guest exceeded ",
        ),
        (
            "an output outside memory",
            "(global.set $output (i32.const 2621400)) (i32.const 4096)",
            "guest failed: guest named 4096 bytes at 2621400 outside its memory",
        ),
    ] {
        let (card, took) = drawn(&other_cards(on_card, &[])?, &request).await?;
        let failure = card.err().with_context(|| format!("{case} was shown"))?;
        assert!(
            format!("{failure:#}").starts_with(refused),
            "{case}: {failure:#}"
        );
        assert!(took < Duration::from_secs(3), "{case} took {took:?}");
    }

    // A request beyond what a module is given is not sent to it.
    let huge = working_card(&"x".repeat(super::super::CARD_REQUEST_LIMIT));
    let (card, took) = drawn(
        &other_cards("(loop $forever (br $forever)) (i32.const 0)", &[])?,
        &huge,
    )
    .await?;
    assert!(
        format!("{:#}", card.err().context("a huge card was shown")?).contains("more than the"),
    );
    assert!(took < Duration::from_millis(500), "{took:?}");
    Ok(())
}

#[tokio::test]
async fn a_card_reaches_no_native_operation() -> anyhow::Result<()> {
    // The module asks to submit text, then shows whether it was refused:
    // `{"Error": …}` is refused, anything else would have been done.
    let submit = String::from_utf8(submit_request()?)?;
    let (refused, done) = (showing("refused"), showing("done"));
    let on_card = format!(
        "(local.set $reply (call $call (i32.const 16384) (i32.const {})))
           (call $take (i32.const 24576) (local.get $reply))
           (if (result i32) (i32.eq (i32.load8_u (i32.const 24578)) (i32.const 69))
             (then (global.set $output (i32.const 20480)) (i32.const {}))
             (else (global.set $output (i32.const 22528)) (i32.const {})))",
        submit.len(),
        refused.len(),
        done.len(),
    );
    let module = other_cards(
        &on_card,
        &[(16384, &submit), (20480, &refused), (22528, &done)],
    )?;
    let (card, _) = drawn(&module, &working_card("Working")).await?;
    assert_eq!(
        card?.and_then(|embed| embed.description).as_deref(),
        Some("refused")
    );
    Ok(())
}

/// The description a card shows, as the serving module draws it.
async fn drawn_by(module: &Module) -> anyhow::Result<Option<String>> {
    Ok(module
        .render_card(&working_card("Working"))
        .await?
        .and_then(|embed| embed.description))
}

#[tokio::test]
async fn a_module_that_cannot_draw_cards_does_not_serve() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let module = release_commands(root, &["model"], "release").await?;
    let describe = describing(&[definition("model")]);
    for card in [
        replying("not a card", &[]),
        r#"{"Failure":"no cards here"}"#.to_owned(),
        json!({"Card": {"Show": {"type": "rich", "description": "x".repeat(4_097)}}}).to_string(),
        // Removing the card of a job at work.
        r#"{"Card":"Remove"}"#.to_owned(),
    ] {
        let candidate = module_with_card(&describe, &replying("candidate", &[]), &card)?;
        std::fs::write(root.join("candidate.wasm"), &candidate)?;
        let declared = custom(root, "candidate.wasm");
        let refused = module.apply(declared.clone(), None).await;
        let Err(ChangeError::Unusable(error)) = refused else {
            anyhow::bail!("a module answering cards with {card} was applied");
        };
        assert!(
            format!("{error:#}").contains("render a card of"),
            "{error:#}"
        );
        assert_eq!(reply(&module).await?, "release");
        // Nor does it serve at startup.
        let unstarted = Module::load(files(root), declared)
            .await
            .err()
            .context("a module that cannot draw cards started")?;
        assert!(
            format!("{unstarted:#}").contains("render a card of"),
            "{unstarted:#}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn cards_are_drawn_by_the_module_serving_when_they_are_drawn() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let describe = describing(&[definition("model")]);
    std::fs::write(
        root.join("release.wasm"),
        module_with_card(
            &describe,
            &replying("release", &[]),
            &showing("release card"),
        )?,
    )?;
    let module = Module::load(files(root), Source::Bundled).await?;
    assert_eq!(drawn_by(&module).await?.as_deref(), Some("release card"));
    std::fs::write(
        root.join("custom.wasm"),
        module_with_card(&describe, &replying("custom", &[]), &showing("custom card"))?,
    )?;
    module.apply(custom(root, "custom.wasm"), None).await?;
    assert_eq!(drawn_by(&module).await?.as_deref(), Some("custom card"));
    // A start with the module declared draws them with it.
    let restarted = Module::load(files(root), custom(root, "custom.wasm")).await?;
    assert_eq!(drawn_by(&restarted).await?.as_deref(), Some("custom card"));
    module.apply(Source::Bundled, None).await?;
    assert_eq!(drawn_by(&module).await?.as_deref(), Some("release card"));
    Ok(())
}

/// The cards the native renderer showed before cards moved into the
/// module, with the requests they were drawn from.
#[derive(serde::Deserialize)]
struct Recorded {
    name: String,
    request: CardRequest,
    card: Option<twilight_model::channel::message::Embed>,
}

#[tokio::test]
async fn the_release_module_draws_what_the_native_renderer_drew() -> anyhow::Result<()> {
    let recorded: Vec<Recorded> = serde_json::from_str(include_str!(
        "../../../../carillon-discord-module/fixtures/cards.json"
    ))?;
    let names = recorded
        .iter()
        .map(|case| case.name.as_str())
        .collect::<Vec<_>>();
    for case in [
        "queued",
        "working",
        "interrupted",
        "earned-summary",
        "removal",
    ] {
        assert!(names.contains(&case), "{case} is not recorded");
    }
    let module = release_module().await?;
    for case in recorded {
        assert_eq!(
            module.render_card(&case.request).await?,
            case.card,
            "{}",
            case.name
        );
    }
    assert_eq!(
        super::super::check(&release_module_bytes()?).await?,
        ["model", "command"]
    );
    Ok(())
}
#[tokio::test]
async fn the_example_module_colours_the_bundled_cards_through_the_host() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::write(root.join("release.wasm"), release_module_bytes()?)?;
    std::fs::write(root.join("custom.wasm"), example_module_bytes()?)?;
    let module = Module::load(files(root), Source::Bundled).await?;
    let bundled = module
        .render_card(&working_card("Working"))
        .await?
        .context("the bundled module drew no card")?;
    assert_eq!(bundled.color, None);

    let change = module.apply(custom(root, "custom.wasm"), None).await?;
    assert_eq!(change.serving.commands, ["model", "command"]);
    let coloured = module
        .render_card(&working_card("Working"))
        .await?
        .context("the example module drew no card")?;
    assert_eq!(coloured.color, Some(0x58_65_F2));
    assert_eq!(coloured.description, bundled.description);
    assert_eq!(coloured.fields, bundled.fields);
    Ok(())
}
