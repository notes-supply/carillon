use super::*;

/// The headline and Reasoning section a card shows of what a model wrote.
fn shown(headline: &str, notes: &[String]) -> (String, String) {
    let mut activity = tools();
    activity.headline = Some(headline.to_owned());
    activity.notes = notes.to_vec();
    let embed = discord(&card(
        &live(&activity, Vec::new()),
        &ActivityMarkers::default(),
    ));
    valid(&embed);
    let quoted = text(&embed)
        .lines()
        .find_map(|line| line.strip_prefix("> "))
        .unwrap_or_default()
        .to_owned();
    (
        quoted,
        field(&embed, "Reasoning").unwrap_or_default().to_owned(),
    )
}

#[test]
fn a_models_emphasis_and_inline_code_render() {
    let (headline, reasoning) = shown(
        "`cargo-sweep` is already installed",
        &["**Investigating scanning issues** I need to look into *why* it scans.".to_owned()],
    );
    assert_eq!(headline, "`cargo-sweep` is already installed");
    assert_eq!(
        reasoning,
        "**Investigating scanning issues** I need to look into *why* it scans."
    );
}

#[test]
fn enclosing_markers_inside_code_cannot_expose_model_markdown() {
    for (input, expected) in [
        (
            "**Note `x** [docs](https://evil.example) y`**",
            r"**Note \`x** \[docs\]\(https\://evil.example\) y\`\*\*",
        ),
        (
            "*Note `x* [docs](https://evil.example) y`*",
            r"*Note \`x* \[docs\]\(https\://evil.example\) y\`\*",
        ),
        (
            "_Note `x_ [docs](https://evil.example) y`_",
            r"_Note \`x_ \[docs\]\(https\://evil.example\) y\`\_",
        ),
        (
            "**Checking `**/*.rs` files**",
            r"**Checking \`**/\*.rs\` files\*\*",
        ),
    ] {
        let (headline, reasoning) = shown(input, &[input.to_owned()]);
        assert_eq!(headline, expected, "{input}");
        assert_eq!(reasoning, expected, "{input}");
    }
    for (input, expected) in [
        ("<@200> @everyone", r"\<\@200\> \@everyone"),
        ("https://evil.example", r"https\://evil.example"),
        ("||secret||", r"\|\|secret\|\|"),
        ("<t:1790000000:R>", r"\<t\:1790000000\:R\>"),
        ("<:emote:123>", r"\<\:emote\:123\>"),
        ("# heading > quote - list", r"\# heading \> quote - list"),
    ] {
        for (marker, escaped) in [("**", r"\*\*"), ("*", r"\*"), ("_", r"\_")] {
            let source = format!("{marker}Note `x{marker} {input} y`{marker}");
            let expected = format!("{marker}Note \\`x{marker} {expected} y\\`{escaped}");
            let (_, reasoning) = shown("Working", std::slice::from_ref(&source));
            assert_eq!(reasoning, expected, "{source}");
        }
    }
}

#[test]
fn carriage_returns_cannot_extend_spans_or_start_lists() {
    let (_, reasoning) = shown(
        "Working",
        &["**one\rtwo**\r- item\r\n`three\rfour`".to_owned()],
    );
    assert_eq!(
        reasoning,
        "\\*\\*one\ntwo\\*\\*\n\\- item\n\\`three\nfour\\`"
    );
}

#[test]
fn model_text_cannot_mention_link_or_structure_anything() {
    let (headline, reasoning) = shown(
        "**bold** <@200> @everyone",
        &["See [here](https://example.invalid) and <#300> `code` # title".to_owned()],
    );
    assert_eq!(headline, r"**bold** \<\@200\> \@everyone");
    assert_eq!(
        reasoning,
        r"See \[here\]\(https\://example.invalid\) and \<\#300\> `code` \# title"
    );
}

#[test]
fn a_span_the_limit_splits_stays_escaped() {
    let (headline, _) = shown(&format!("{} **bold words**", "x".repeat(90)), &[]);
    assert_eq!(headline, format!(r"{} \*\*bold w…", "x".repeat(90)));
    // Older notes give way, and the newest is cut inside its second span.
    let notes = [
        "a".repeat(600),
        format!("{} **kept** **split here**", "b".repeat(490)),
    ];
    let (_, reasoning) = shown("Working", &notes);
    assert!(reasoning.starts_with("… 1 more above\n"), "{reasoning}");
    assert!(reasoning.contains(" **kept** "), "{reasoning}");
    assert!(reasoning.contains(r" \*\*split"), "{reasoning}");
    assert!(reasoning.ends_with('…'), "{reasoning}");
}
