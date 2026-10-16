use super::prose;

#[test]
fn balanced_emphasis_and_code_on_one_line_render_as_written() {
    for text in [
        "**Evaluating script needs** I need to assess the limits.",
        "an *italic* and an _italic_ word",
        "**bold with `code` and *italic* inside**",
        "`<@200> https://example.invalid ||not hidden||` shows literally",
    ] {
        assert_eq!(prose(text), text);
    }
}

#[test]
fn unbalanced_or_unsupported_markers_stay_escaped() {
    for (text, shown) in [
        ("**unfinished bold", r"\*\*unfinished bold"),
        ("a * b * c", r"a \* b \* c"),
        ("snake_case_name", r"snake\_case\_name"),
        ("`unclosed code", r"\`unclosed code"),
        ("``doubled``", r"\`\`doubled\`\`"),
        ("**bold\nacross lines**", "\\*\\*bold\nacross lines\\*\\*"),
        ("***both***", r"\*\*\*both\*\*\*"),
        (
            "~~struck~~ __underlined__",
            r"\~\~struck\~\~ \_\_underlined\_\_",
        ),
        (r"already \*escaped\*", r"already \\\*escaped\\\*"),
    ] {
        assert_eq!(prose(text), shown, "{text}");
    }
}

#[test]
fn model_text_never_mentions_links_or_structures_anything() {
    for (construct, text, shown) in [
        (
            "mentions",
            "<@200> <@&300> <#400> @everyone @here",
            r"\<\@200\> \<\@&300\> \<\#400\> \@everyone \@here",
        ),
        (
            "a masked link",
            "[docs](https://example.invalid)",
            r"\[docs\]\(https\://example.invalid\)",
        ),
        (
            "URLs",
            "https://example.invalid <https://example.invalid>",
            r"https\://example.invalid \<https\://example.invalid\>",
        ),
        ("a heading", "# Title", r"\# Title"),
        ("subtext", "-# small", r"\-\# small"),
        ("a block quote", ">>> quoted", r"\>\>\> quoted"),
        (
            "lists",
            "- one\n* two\n+ three\n1. four\n  2) five",
            "\\- one\n\\* two\n\\+ three\n1\\. four\n  2\\) five",
        ),
        (
            "a code fence",
            "```rust\nfn main() {}\n```",
            "\\`\\`\\`rust\nfn main\\(\\) {}\n\\`\\`\\`",
        ),
        ("a spoiler", "||secret||", r"\|\|secret\|\|"),
        (
            "custom emoji",
            "<:claude:123> <a:spin:456>",
            r"\<\:claude\:123\> \<a\:spin\:456\>",
        ),
        ("a timestamp", "<t:1790000000:R>", r"\<t\:1790000000\:R\>"),
    ] {
        assert_eq!(prose(text), shown, "{construct}");
    }
}
