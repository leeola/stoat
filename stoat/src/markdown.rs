//! Render markdown into styled lines for the LSP hover popup.
//!
//! Language servers return hover documentation as markdown. This module turns
//! that source into a list of lines, each a list of `(text, style)` spans, so
//! the hover renderer paints headings, emphasis, code, and lists with theme
//! colors instead of showing raw `**`/backtick syntax.

use crate::{display_map::syntax_theme::theme_scope_for_id, theme::Theme};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::Style;
use std::sync::Arc;
use stoat_language::{Language, LanguageRegistry};

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
/// Fenced code blocks are syntax-highlighted when the fence names a language in
/// `languages`, otherwise their text is styled literal. Strikethrough is the
/// only non-default parser extension enabled.
pub(crate) fn render_markdown(
    text: &str,
    theme: &Theme,
    languages: &LanguageRegistry,
) -> Vec<StyledLine> {
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
    let mut code_buf = String::new();

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
                // A code block accumulates its whole text so tree-sitter can
                // parse it. On close, highlight and emit it before the blank.
                if matches!(tag, TagEnd::CodeBlock) {
                    let language = code_block_language(&tags, languages);
                    let code = std::mem::take(&mut code_buf);
                    lines.extend(emit_code_block(&code, language.as_deref(), literal, theme));
                }
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
                    code_buf.push_str(&text);
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

/// Resolve a fenced code block's language from the open [`Tag::CodeBlock`],
/// matching its fence token against each language's name and extensions.
fn code_block_language(tags: &[Tag<'_>], languages: &LanguageRegistry) -> Option<Arc<Language>> {
    let Some(Tag::CodeBlock(CodeBlockKind::Fenced(token))) = tags.last() else {
        return None;
    };
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    languages
        .languages()
        .iter()
        .find(|lang| {
            lang.name.eq_ignore_ascii_case(token)
                || lang
                    .extensions
                    .iter()
                    .any(|ext| ext.eq_ignore_ascii_case(token))
        })
        .cloned()
}

/// Highlight `code` as `language`, or style it literal when `language` is
/// `None`, returning one styled line per source line with tabs expanded to four
/// spaces.
fn emit_code_block(
    code: &str,
    language: Option<&Language>,
    literal: Style,
    theme: &Theme,
) -> Vec<StyledLine> {
    let spans = match language {
        Some(lang) => stoat_language::parse(lang, code, None)
            .map(|tree| stoat_language::extract_highlights(lang, &tree, code))
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // Seed every byte with the literal style, then let each span overwrite its
    // range. extract_highlights sorts more-specific captures later, so a nested
    // capture wins over the broader one it sits inside.
    let mut byte_styles = vec![literal; code.len()];
    for span in &spans {
        let style = style_for_id(span.id, literal, theme);
        for byte in span.byte_range.clone() {
            if let Some(slot) = byte_styles.get_mut(byte) {
                *slot = style;
            }
        }
    }

    let mut lines: Vec<StyledLine> = Vec::new();
    let mut current: StyledLine = Vec::new();
    let mut run = String::new();
    let mut run_style = literal;
    for (byte, ch) in code.char_indices() {
        if ch == '\n' {
            if !run.is_empty() {
                current.push((std::mem::take(&mut run), run_style));
            }
            lines.push(std::mem::take(&mut current));
            continue;
        }
        let style = byte_styles.get(byte).copied().unwrap_or(literal);
        if !run.is_empty() && style != run_style {
            current.push((std::mem::take(&mut run), run_style));
        }
        if run.is_empty() {
            run_style = style;
        }
        if ch == '\t' {
            run.push_str("    ");
        } else {
            run.push(ch);
        }
    }
    if !run.is_empty() {
        current.push((run, run_style));
    }
    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

/// Resolve a highlight span's id to a style layered over the literal base,
/// keeping the literal style for [`stoat_language::HighlightId::DEFAULT`].
fn style_for_id(id: stoat_language::HighlightId, literal: Style, theme: &Theme) -> Style {
    match theme_scope_for_id(id) {
        Some(scope) => literal.patch(theme.get(&scope)),
        None => literal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_map::syntax_theme::SyntaxStyles;
    use ratatui::style::{Color, Modifier};
    use stoat_config::parse;
    use stoat_language::HighlightMap;

    fn theme() -> Theme {
        let src = r##"theme t {
            syntax.markup.title.fg = "#010101";
            syntax.markup.emphasis = { modifiers: [italic] };
            syntax.markup.emphasis.strong = { modifiers: [bold] };
            syntax.markup.strikethrough = { modifiers: [strikethrough] };
            syntax.markup.text.literal.fg = "#020202";
            syntax.markup.link_text.fg = "#030303";
            syntax.punctuation.special.fg = "#040404";
            syntax.keyword.fg = "#050607";
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

    /// A registry with no highlight maps installed, so a fenced block renders
    /// literal (every capture resolves to the default id).
    fn registry() -> LanguageRegistry {
        LanguageRegistry::standard()
    }

    #[test]
    fn heading_uses_the_title_scope() {
        assert_eq!(
            render_markdown("# Title", &theme(), &registry()),
            vec![vec![("Title".to_string(), rgb(1, 1, 1))]]
        );
    }

    #[test]
    fn bold_uses_the_strong_scope() {
        assert_eq!(
            render_markdown("**bold**", &theme(), &registry()),
            vec![vec![(
                "bold".to_string(),
                Style::default().add_modifier(Modifier::BOLD)
            )]]
        );
    }

    #[test]
    fn inline_code_uses_the_literal_scope() {
        assert_eq!(
            render_markdown("`code`", &theme(), &registry()),
            vec![vec![("code".to_string(), rgb(2, 2, 2))]]
        );
    }

    #[test]
    fn unordered_list_prefixes_each_item_with_a_bullet() {
        assert_eq!(
            render_markdown("- a\n- b", &theme(), &registry()),
            vec![
                vec![plain(BULLET), plain("a")],
                vec![plain(BULLET), plain("b")],
            ]
        );
    }

    #[test]
    fn fenced_block_styles_each_line_literal() {
        assert_eq!(
            render_markdown("```rust\nfn foo() {}\n```", &theme(), &registry()),
            vec![vec![("fn foo() {}".to_string(), rgb(2, 2, 2))]]
        );
    }

    /// A registry whose languages have highlight maps installed against `theme`,
    /// so fenced blocks are syntax-highlighted (mirrors the host wiring).
    fn highlighted_registry(theme: &Theme) -> LanguageRegistry {
        let registry = LanguageRegistry::standard();
        let styles = SyntaxStyles::from_theme(theme);
        for lang in registry.languages() {
            lang.set_highlight_map(HighlightMap::new(
                lang.highlight_capture_names(),
                styles.theme_keys(),
            ));
        }
        registry
    }

    #[test]
    fn fenced_rust_highlights_keywords() {
        let theme = theme();
        let registry = highlighted_registry(&theme);

        let literal = theme.get("syntax.markup.text.literal");
        let keyword = theme.get("syntax.keyword");
        assert_ne!(
            keyword, literal,
            "theme must color the keyword scope distinctly"
        );

        let lines = render_markdown("```rust\nfn x() {}\n```", &theme, &registry);
        assert_eq!(
            lines[0][0],
            ("fn".to_string(), literal.patch(keyword)),
            "the fn keyword is styled from the keyword scope over the literal base"
        );
    }
}
