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
fn code_inside_emphasis_keeps_its_literal_markers() {
    let (headline, reasoning) = shown(
        "**Checking `**/*.rs` files**",
        &["*Compare `a*b` with `a_b`* and **keep `**` literal**".to_owned()],
    );
    assert_eq!(headline, "**Checking `**/*.rs` files**");
    assert_eq!(
        reasoning,
        "*Compare `a*b` with `a_b`* and **keep `**` literal**"
    );
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
