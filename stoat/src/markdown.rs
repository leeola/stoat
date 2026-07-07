//! Render markdown into styled lines for the LSP hover popup.
//!
//! Language servers return hover documentation as markdown. This module turns
//! that source into a list of lines, each a list of `(text, style)` spans, so
//! the hover renderer paints headings, emphasis, code, and lists with theme
//! colors instead of showing raw `**`/backtick syntax.

use crate::theme::Theme;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::Style;

/// One rendered line as an ordered list of styled spans. An empty list is a
/// blank line.
type StyledLine = Vec<(String, Style)>;

/// Two spaces of indent per markdown list nesting level.
const INDENT: &str = "  ";

/// The unordered-list item prefix, a U+2022 BULLET followed by a space.
const BULLET: &str = "\u{2022} ";

/// A horizontal rule, drawn as three U+2500 box-drawing dashes.
const RULE: &str = "\u{2500}\u{2500}\u{2500}";

/// Render markdown `text` into styled lines resolved against `theme`.
///
/// Each line is a list of `(text, style)` spans, and an empty span list is a
/// blank line. Plain text carries [`Style::default`] so the caller supplies the
/// base foreground. Headings, emphasis, inline code, links, and rules carry
/// their theme scope's style. Only the innermost markdown tag styles a span, so
/// nested emphasis inside a heading takes the emphasis style, matching the
/// reference renderer.
///
/// Code-block lines are styled as literal text. Syntax highlighting is applied
/// by the caller in a later pass. Strikethrough is the only non-default parser
/// extension enabled.
pub(crate) fn render_markdown(text: &str, theme: &Theme) -> Vec<StyledLine> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let title = theme.get("syntax.markup.title");
    let emphasis = theme.get("syntax.markup.emphasis");
    let strong = theme.get("syntax.markup.emphasis.strong");
    let strikethrough = theme.get("syntax.markup.strikethrough");
    let literal = theme.get("syntax.markup.text.literal");
    let link = theme.get("syntax.markup.link_text");
    let rule = theme.get("syntax.punctuation.special");

    let mut tags = Vec::new();
    let mut spans: StyledLine = Vec::new();
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::List(list)) => {
                if !list_stack.is_empty() {
                    push_line(&mut spans, &mut lines);
                }
                list_stack.push(list);
            },
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() {
                    lines.push(Vec::new());
                }
            },
            Event::Start(Tag::Item) => {
                tags.push(Tag::Item);
                let bullet = match list_stack.last().copied().flatten() {
                    Some(n) => format!("{n}. "),
                    None => BULLET.to_string(),
                };
                if let Some(Some(n)) = list_stack.last_mut() {
                    *n += 1;
                }
                spans.push((
                    format!("{}{bullet}", indent(list_stack.len())),
                    Style::default(),
                ));
            },
            Event::Start(tag) => {
                tags.push(tag);
                if spans.is_empty() && !list_stack.is_empty() {
                    spans.push((indent(list_stack.len()), Style::default()));
                }
            },
            Event::End(tag) => {
                tags.pop();
                if matches!(
                    tag,
                    TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock | TagEnd::Item
                ) {
                    push_line(&mut spans, &mut lines);
                }
                if matches!(
                    tag,
                    TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock
                ) {
                    lines.push(Vec::new());
                }
            },
            Event::Text(text) => {
                if matches!(tags.last(), Some(Tag::CodeBlock(_))) {
                    for line in text.lines() {
                        lines.push(vec![(line.to_string(), literal)]);
                    }
                } else {
                    let style = match tags.last() {
                        Some(Tag::Heading { .. }) => title,
                        Some(Tag::Emphasis) => emphasis,
                        Some(Tag::Strong) => strong,
                        Some(Tag::Strikethrough) => strikethrough,
                        Some(Tag::Link { .. }) => link,
                        _ => Style::default(),
                    };
                    spans.push((text.to_string(), style));
                }
            },
            Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                spans.push((text.to_string(), literal));
            },
            Event::SoftBreak | Event::HardBreak => {
                push_line(&mut spans, &mut lines);
                if !list_stack.is_empty() {
                    spans.push((indent(list_stack.len()), Style::default()));
                }
            },
            Event::Rule => {
                lines.push(vec![(RULE.to_string(), rule)]);
                lines.push(Vec::new());
            },
            _ => {},
        }
    }

    if !spans.is_empty() {
        lines.push(spans);
    }
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    lines
}

/// Move the accumulated spans onto `lines` as one line, dropping an empty run.
fn push_line(spans: &mut StyledLine, lines: &mut Vec<StyledLine>) {
    let line = std::mem::take(spans);
    if !line.is_empty() {
        lines.push(line);
    }
}

/// The indent for a list nested `level` deep. The top level gets none, then two
/// spaces per level below it.
fn indent(level: usize) -> String {
    if level < 1 {
        String::new()
    } else {
        INDENT.repeat(level - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};
    use stoat_config::parse;

    fn theme() -> Theme {
        let src = r##"theme t {
            syntax.markup.title.fg = "#010101";
            syntax.markup.emphasis = { modifiers: [italic] };
            syntax.markup.emphasis.strong = { modifiers: [bold] };
            syntax.markup.strikethrough = { modifiers: [strikethrough] };
            syntax.markup.text.literal.fg = "#020202";
            syntax.markup.link_text.fg = "#030303";
            syntax.punctuation.special.fg = "#040404";
        }"##;
        let (config, errors) = parse(src);
        assert!(errors.is_empty(), "theme parse errors: {errors:?}");
        Theme::from_config(&config.expect("config"), "t").expect("theme")
    }

    fn rgb(r: u8, g: u8, b: u8) -> Style {
        Style::default().fg(Color::Rgb(r, g, b))
    }

    fn plain(text: &str) -> (String, Style) {
        (text.to_string(), Style::default())
    }

    #[test]
    fn heading_uses_the_title_scope() {
        assert_eq!(
            render_markdown("# Title", &theme()),
            vec![vec![("Title".to_string(), rgb(1, 1, 1))]]
        );
    }

    #[test]
    fn bold_uses_the_strong_scope() {
        assert_eq!(
            render_markdown("**bold**", &theme()),
            vec![vec![(
                "bold".to_string(),
                Style::default().add_modifier(Modifier::BOLD)
            )]]
        );
    }

    #[test]
    fn inline_code_uses_the_literal_scope() {
        assert_eq!(
            render_markdown("`code`", &theme()),
            vec![vec![("code".to_string(), rgb(2, 2, 2))]]
        );
    }

    #[test]
    fn unordered_list_prefixes_each_item_with_a_bullet() {
        assert_eq!(
            render_markdown("- a\n- b", &theme()),
            vec![
                vec![plain(BULLET), plain("a")],
                vec![plain(BULLET), plain("b")],
            ]
        );
    }

    #[test]
    fn fenced_block_styles_each_line_literal() {
        assert_eq!(
            render_markdown("```rust\nfn foo() {}\n```", &theme()),
            vec![vec![("fn foo() {}".to_string(), rgb(2, 2, 2))]]
        );
    }
}
