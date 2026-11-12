//! Answers as HTML the page inserts as given. The page parses no Markdown
//! and never sees markup a model wrote: this is the one place that decides
//! what markup an answer makes.

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd, html};

/// `markdown` as HTML that runs nothing and loads nothing. Raw HTML is
/// shown as the text it is. A link stays one only to `http`, `https` or
/// `mailto`, and otherwise leaves its text. An image becomes a link to it:
/// the page's policy loads no image from elsewhere, and an answer's own
/// files come with the turn.
pub(crate) fn render(markdown: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut became_anchor = Vec::new();
    let events = Parser::new_ext(markdown, options).filter_map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
        Event::Start(
            Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }
            | Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            },
        ) => {
            let kept = !became_anchor.contains(&true)
                && (link_type == LinkType::Email || allowed(&dest_url));
            became_anchor.push(kept);
            kept.then_some(Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }))
        }
        Event::End(TagEnd::Link | TagEnd::Image) => became_anchor
            .pop()
            .unwrap_or_default()
            .then_some(Event::End(TagEnd::Link)),
        event => Some(event),
    });
    let mut rendered = String::new();
    html::push_html(&mut rendered, events);
    // Every `<` of the source is escaped by now, so each tag here is one the
    // renderer wrote. It aligns a table's column with an inline style, which
    // the page's policy refuses, so the alignment becomes a class the page's
    // stylesheet knows.
    rendered
        .replace(
            "<a href=",
            "<a target=\"_blank\" rel=\"noreferrer noopener\" href=",
        )
        .replace("<th style=\"text-align: ", "<th class=\"align-")
        .replace("<td style=\"text-align: ", "<td class=\"align-")
}

fn allowed(url: &str) -> bool {
    ["http://", "https://", "mailto:"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    })
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn raw_html_is_shown_as_text_and_never_as_markup() {
        for (source, shown) in [
            (
                "<script>alert(1)</script>",
                "&lt;script&gt;alert(1)&lt;/script&gt;",
            ),
            (
                "before <img src=x onerror=alert(1)> after",
                "&lt;img src=x onerror=alert(1)&gt;",
            ),
            ("<a href=\"https://example.com\">x</a>", "&lt;a href="),
            ("<!-- note --><style>p{}</style>", "&lt;style&gt;"),
            ("<iframe src=\"//evil.example\"></iframe>", "&lt;iframe"),
        ] {
            let rendered = render(source);
            assert!(rendered.contains(shown), "{source} became {rendered}");
            assert!(
                !rendered.contains("<script") && !rendered.contains("<img"),
                "{rendered}"
            );
            assert!(!rendered.contains("<iframe") && !rendered.contains("<style"));
            assert!(!rendered.contains("<a "), "{rendered}");
        }
    }

    #[test]
    fn a_table_s_alignment_is_a_class_and_never_an_inline_style() {
        let rendered = render(
            "| Day | Turns | Note |\n| :-- | --: | :-: |\n\
             | Monday | 58 | say style=\"text-align: right\" |",
        );
        for cell in [
            "<th class=\"align-left\">Day</th>",
            "<td class=\"align-right\">58</td>",
            "<th class=\"align-center\">Note</th>",
            ">say style=\"text-align: right\"</td>",
        ] {
            assert!(rendered.contains(cell), "{cell} is not in {rendered}");
        }
        assert!(!rendered.contains("<td style=") && !rendered.contains("<th style="));
    }

    #[test]
    fn a_link_survives_only_to_the_web_or_to_mail() {
        for source in [
            "[run](javascript:alert(1))",
            "[run](JaVaScRiPt:alert(1))",
            "[data](data:text/html,<script>alert(1)</script>)",
            "[file](file:///etc/passwd)",
            "[relative](/api/session)",
            "[other](//evil.example/x)",
            "![pixel](javascript:alert(1))",
            "<javascript:alert(1)>",
            "[ref]\n\n[ref]: vbscript:x",
        ] {
            let rendered = render(source);
            assert!(!rendered.contains("<a "), "{source} became {rendered}");
            assert!(!rendered.contains("<img"), "{source} became {rendered}");
        }
        assert_eq!(render("[run](javascript:alert(1))"), "<p>run</p>\n");
        assert_eq!(
            render("[site](https://example.com/?a=1&b=\"2\") and <me@example.com>"),
            "<p><a target=\"_blank\" rel=\"noreferrer noopener\" \
             href=\"https://example.com/?a=1&amp;b=%222%22\">site</a> and \
             <a target=\"_blank\" rel=\"noreferrer noopener\" \
             href=\"mailto:me@example.com\">me@example.com</a></p>\n"
        );
        assert_eq!(
            render("![a chart](http://example.com/chart.png)"),
            "<p><a target=\"_blank\" rel=\"noreferrer noopener\" \
             href=\"http://example.com/chart.png\">a chart</a></p>\n"
        );
        // An image inside a link is that link's text, not a second anchor.
        assert_eq!(
            render("[![alt](https://example.com/a.png)](https://example.com)"),
            "<p><a target=\"_blank\" rel=\"noreferrer noopener\" \
             href=\"https://example.com\">alt</a></p>\n"
        );
    }

    #[test]
    fn ordinary_markdown_renders() {
        assert_eq!(
            render("# Title\n\nSome *emphasis*, **strength**, ~~struck~~ and `code`.\n"),
            "<h1>Title</h1>\n<p>Some <em>emphasis</em>, <strong>strength</strong>, \
             <del>struck</del> and <code>code</code>.</p>\n"
        );
        assert_eq!(
            render("```rust\nlet a = 1 < 2;\n```\n"),
            "<pre><code class=\"language-rust\">let a = 1 &lt; 2;\n</code></pre>\n"
        );
        let list = render("- one\n- [x] done\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(list.contains("<li>one</li>"), "{list}");
        assert!(list.contains("type=\"checkbox\""), "{list}");
        assert!(
            list.contains("<table>") && list.contains("<td>2</td>"),
            "{list}"
        );
        assert_eq!(
            render("`<a href=x>`"),
            "<p><code>&lt;a href=x&gt;</code></p>\n"
        );
    }
}
