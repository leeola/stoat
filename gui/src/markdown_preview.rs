//! Markdown preview pane.
//!
//! Renders the source buffer's markdown as a read-only side pane,
//! re-parsing on every buffer edit. `OpenMarkdownPreview` splits the
//! active pane and opens a preview targeting the active editor's
//! buffer. The buffer text is parsed into a small block/inline IR
//! ([`parse_markdown`]) which the render walks into gpui elements;
//! fenced code blocks are syntax-highlighted through the fence
//! language's tree-sitter grammar.

use crate::{
    buffer::{Buffer, BufferEvent},
    editor::render::convert_highlight_style,
    globals::LanguageRegistry,
    item::{DeserializeSnafu, ItemError, ItemKind, ItemView},
    theme::{ActiveTheme, Theme},
};
use gpui::{
    div, px, rems, App, Context, Entity, FontStyle, FontWeight, HighlightStyle, IntoElement,
    ParentElement, Render, SharedString, Styled, StyledText, Subscription, Window,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};
use serde_json::Value;
use std::{ops::Range, path::Path, sync::Arc};
use stoat::display_map::syntax_theme::SyntaxStyles;
use stoat_language::{HighlightMap, Language, SyntaxMap};
use stoat_text::Rope;

/// Inline markdown span. Emphasis/strong/link nest other spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdInline {
    Text(String),
    Code(String),
    Emphasis(Vec<MdInline>),
    Strong(Vec<MdInline>),
    Link { content: Vec<MdInline>, url: String },
}

/// Block-level markdown element. List items are flattened with an
/// explicit `depth` and `ordered` flag rather than nested, so the
/// render can indent without recursing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdBlock {
    Heading {
        level: u8,
        content: Vec<MdInline>,
    },
    Paragraph(Vec<MdInline>),
    Code {
        lang: Option<String>,
        code: String,
    },
    ListItem {
        depth: usize,
        ordered: bool,
        content: Vec<MdInline>,
    },
    Rule,
}

/// Parse `text` into a flat list of block elements. Images, tables, and
/// raw HTML are skipped; their textual content still surfaces where the
/// parser emits plain text events.
pub fn parse_markdown(text: &str) -> Vec<MdBlock> {
    let mut events = Parser::new(text).peekable();
    let mut out = Vec::new();
    parse_blocks(&mut events, &mut out);
    out
}

type Events<'a> = std::iter::Peekable<Parser<'a>>;

fn parse_blocks(events: &mut Events<'_>, out: &mut Vec<MdBlock>) {
    while let Some(event) = events.peek() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let level = heading_level(*level);
                events.next();
                let content = parse_inline(events);
                consume_end(events);
                out.push(MdBlock::Heading { level, content });
            },
            Event::Start(Tag::Paragraph) => {
                events.next();
                let content = parse_inline(events);
                consume_end(events);
                out.push(MdBlock::Paragraph(content));
            },
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(tag) if !tag.is_empty() => Some(tag.to_string()),
                    _ => None,
                };
                events.next();
                let mut code = String::new();
                while let Some(ev) = events.peek() {
                    match ev {
                        Event::Text(t) => {
                            code.push_str(t);
                            events.next();
                        },
                        Event::End(TagEnd::CodeBlock) => {
                            events.next();
                            break;
                        },
                        _ => {
                            events.next();
                        },
                    }
                }
                out.push(MdBlock::Code { lang, code });
            },
            Event::Start(Tag::List(start)) => {
                let ordered = start.is_some();
                events.next();
                parse_list(events, ordered, 0, out);
            },
            Event::Rule => {
                events.next();
                out.push(MdBlock::Rule);
            },
            Event::End(_) => return,
            _ => {
                events.next();
            },
        }
    }
}

fn parse_list(events: &mut Events<'_>, ordered: bool, depth: usize, out: &mut Vec<MdBlock>) {
    while let Some(event) = events.peek() {
        match event {
            Event::Start(Tag::Item) => {
                events.next();
                parse_item(events, ordered, depth, out);
            },
            Event::End(TagEnd::List(_)) => {
                events.next();
                return;
            },
            _ => {
                events.next();
            },
        }
    }
}

fn parse_item(events: &mut Events<'_>, ordered: bool, depth: usize, out: &mut Vec<MdBlock>) {
    let mut content = Vec::new();
    let mut emitted = false;
    let flush = |content: &mut Vec<MdInline>, out: &mut Vec<MdBlock>, emitted: &mut bool| {
        if !*emitted {
            out.push(MdBlock::ListItem {
                depth,
                ordered,
                content: std::mem::take(content),
            });
            *emitted = true;
        }
    };
    while let Some(event) = events.peek() {
        match event {
            Event::End(TagEnd::Item) => {
                events.next();
                flush(&mut content, out, &mut emitted);
                return;
            },
            Event::Start(Tag::List(start)) => {
                let nested_ordered = start.is_some();
                flush(&mut content, out, &mut emitted);
                events.next();
                parse_list(events, nested_ordered, depth + 1, out);
            },
            Event::Start(Tag::Paragraph) => {
                events.next();
                content.extend(parse_inline(events));
                consume_end(events);
            },
            Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Start(Tag::Emphasis | Tag::Strong | Tag::Link { .. }) => {
                content.extend(parse_inline(events));
            },
            _ => {
                events.next();
            },
        }
    }
    flush(&mut content, out, &mut emitted);
}

/// Read inline events until a block-level boundary or `End`, which is
/// left unconsumed for the caller. Nested emphasis/strong/link recurse
/// and consume their own closing `End`.
fn parse_inline(events: &mut Events<'_>) -> Vec<MdInline> {
    let mut out = Vec::new();
    loop {
        match events.peek() {
            Some(Event::Text(_)) => {
                if let Some(Event::Text(t)) = events.next() {
                    out.push(MdInline::Text(t.to_string()));
                }
            },
            Some(Event::Code(_)) => {
                if let Some(Event::Code(t)) = events.next() {
                    out.push(MdInline::Code(t.to_string()));
                }
            },
            Some(Event::SoftBreak) | Some(Event::HardBreak) => {
                events.next();
                out.push(MdInline::Text(" ".to_string()));
            },
            Some(Event::Start(Tag::Emphasis)) => {
                events.next();
                let inner = parse_inline(events);
                consume_end(events);
                out.push(MdInline::Emphasis(inner));
            },
            Some(Event::Start(Tag::Strong)) => {
                events.next();
                let inner = parse_inline(events);
                consume_end(events);
                out.push(MdInline::Strong(inner));
            },
            Some(Event::Start(Tag::Link { dest_url, .. })) => {
                let url = dest_url.to_string();
                events.next();
                let inner = parse_inline(events);
                consume_end(events);
                out.push(MdInline::Link {
                    content: inner,
                    url,
                });
            },
            _ => return out,
        }
    }
}

fn consume_end(events: &mut Events<'_>) {
    if matches!(events.peek(), Some(Event::End(_))) {
        events.next();
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Resolve a fenced code block's language tag (e.g. `rust` or `rs`) to a
/// registered language, trying an exact name match first, then an
/// extension match via a synthetic `code.<tag>` path.
fn lookup_language(
    registry: &stoat_language::LanguageRegistry,
    tag: &str,
) -> Option<Arc<Language>> {
    if let Some(lang) = registry.find_by_name(tag) {
        return Some(lang);
    }
    registry.for_path(Path::new(&format!("code.{tag}")))
}

/// Tree-sitter highlight runs over a fenced code block. Builds a one-off
/// syntax map for `code` in the fence language and maps each capture to a
/// gpui style via a theme-seeded [`HighlightMap`], mirroring the editor's
/// `apply_syntax_overlay`. Empty when the language is unknown, the parse
/// fails, or the theme defines no style for any capture.
fn highlight_code_runs(
    code: &str,
    tag: &str,
    registry: &stoat_language::LanguageRegistry,
    theme: &stoat::theme::Theme,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(language) = lookup_language(registry, tag) else {
        return Vec::new();
    };
    let styles = SyntaxStyles::from_theme(theme);
    let highlight_map = HighlightMap::new(language.highlight_capture_names(), styles.theme_keys());
    let rope = Rope::from(code);
    let mut map = SyntaxMap::new();
    if map.reparse(&rope, language, 0).is_none() {
        return Vec::new();
    }
    let mut runs = Vec::new();
    let snapshot = map.snapshot();
    for capture in snapshot.captures(0..code.len(), &rope, |lang| Some(&lang.highlight_query)) {
        let highlight_id = highlight_map.get(capture.index);
        let Some(style_id) = styles.id_for_highlight(highlight_id) else {
            continue;
        };
        let gpui_style = convert_highlight_style(&styles.interner[style_id]);
        let range = capture.node.byte_range();
        runs.push((range, gpui_style));
    }
    runs
}

pub struct MarkdownPreview {
    buffer: Entity<Buffer>,
    _subscription: Subscription,
}

impl MarkdownPreview {
    pub fn new(buffer: Entity<Buffer>, cx: &mut Context<'_, Self>) -> Self {
        let subscription = cx.subscribe(&buffer, |_, _, event: &BufferEvent, cx| match event {
            BufferEvent::Edited | BufferEvent::Reloaded => cx.notify(),
            _ => {},
        });
        Self {
            buffer,
            _subscription: subscription,
        }
    }
}

/// Flatten an inline tree into a single string plus styled runs over it.
/// The accumulated `style` carries nested emphasis/strong/link styling
/// down to each leaf.
fn inline_runs(
    spans: &[MdInline],
    style: HighlightStyle,
    link_color: gpui::Hsla,
    code_color: gpui::Hsla,
    text: &mut String,
    runs: &mut Vec<(Range<usize>, HighlightStyle)>,
) {
    for span in spans {
        match span {
            MdInline::Text(t) => {
                let start = text.len();
                text.push_str(t);
                runs.push((start..text.len(), style));
            },
            MdInline::Code(t) => {
                let start = text.len();
                text.push_str(t);
                let mut s = style;
                s.color = Some(code_color);
                runs.push((start..text.len(), s));
            },
            MdInline::Emphasis(inner) => {
                let mut s = style;
                s.font_style = Some(FontStyle::Italic);
                inline_runs(inner, s, link_color, code_color, text, runs);
            },
            MdInline::Strong(inner) => {
                let mut s = style;
                s.font_weight = Some(FontWeight::BOLD);
                inline_runs(inner, s, link_color, code_color, text, runs);
            },
            MdInline::Link { content, .. } => {
                let mut s = style;
                s.color = Some(link_color);
                s.underline = Some(gpui::UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(link_color),
                    wavy: false,
                });
                inline_runs(content, s, link_color, code_color, text, runs);
            },
        }
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let text_color = theme.statusbar_text;
        let muted = theme.muted_text;
        let link_color = theme.modal_palette;
        let code_block_bg = theme.sticky_header_background;
        let blocks = parse_markdown(&self.buffer.read(cx).text());

        let mut container = div().flex().flex_col().size_full().p_2().gap_1();
        for block in blocks {
            let element = match block {
                MdBlock::Heading { level, content } => {
                    let base = HighlightStyle {
                        color: Some(text_color),
                        font_weight: Some(FontWeight::BOLD),
                        ..Default::default()
                    };
                    let (text, runs) = build_runs(&content, base, link_color, muted);
                    let size = rems(1.6 - 0.12 * (level.min(6) - 1) as f32);
                    div()
                        .text_size(size)
                        .child(StyledText::new(text).with_highlights(runs))
                },
                MdBlock::Paragraph(content) => {
                    let base = HighlightStyle {
                        color: Some(text_color),
                        ..Default::default()
                    };
                    let (text, runs) = build_runs(&content, base, link_color, muted);
                    div().child(StyledText::new(text).with_highlights(runs))
                },
                MdBlock::Code { lang, code } => {
                    let runs = lang.as_deref().and_then(|tag| {
                        let registry = cx.try_global::<LanguageRegistry>()?;
                        let theme = cx.try_global::<Theme>()?;
                        let runs = highlight_code_runs(&code, tag, &registry.0, &theme.0);
                        (!runs.is_empty()).then_some(runs)
                    });
                    let block = div()
                        .font_family("monospace")
                        .bg(code_block_bg)
                        .px_2()
                        .py_1()
                        .text_color(text_color);
                    match runs {
                        Some(runs) => block
                            .child(StyledText::new(SharedString::from(code)).with_highlights(runs)),
                        None => block.child(SharedString::from(code)),
                    }
                },
                MdBlock::ListItem {
                    depth,
                    ordered,
                    content,
                } => {
                    let base = HighlightStyle {
                        color: Some(text_color),
                        ..Default::default()
                    };
                    let marker = if ordered { "1." } else { "-" };
                    let (text, runs) = build_runs(&content, base, link_color, muted);
                    div()
                        .pl(rems(1.0 + depth as f32))
                        .flex()
                        .gap_1()
                        .child(div().text_color(muted).child(SharedString::from(marker)))
                        .child(StyledText::new(text).with_highlights(runs))
                },
                MdBlock::Rule => div().h(px(1.0)).bg(muted),
            };
            container = container.child(element);
        }
        container
    }
}

fn build_runs(
    content: &[MdInline],
    base: HighlightStyle,
    link_color: gpui::Hsla,
    code_color: gpui::Hsla,
) -> (SharedString, Vec<(Range<usize>, HighlightStyle)>) {
    let mut text = String::new();
    let mut runs = Vec::new();
    inline_runs(content, base, link_color, code_color, &mut text, &mut runs);
    (SharedString::from(text), runs)
}

impl ItemView for MarkdownPreview {
    fn tab_label(&self, _cx: &App) -> SharedString {
        "Preview".into()
    }

    fn item_kind(&self) -> ItemKind {
        ItemKind::MarkdownPreview
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError>
    where
        Self: Sized,
    {
        DeserializeSnafu {
            reason: "MarkdownPreview is transient and not persisted",
        }
        .fail()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> MdInline {
        MdInline::Text(s.to_string())
    }

    #[test]
    fn parses_heading_with_level() {
        let blocks = parse_markdown("## Title here\n");
        assert_eq!(
            blocks,
            vec![MdBlock::Heading {
                level: 2,
                content: vec![text("Title here")],
            }]
        );
    }

    #[test]
    fn parses_paragraph_with_inline_styles() {
        let blocks = parse_markdown("plain *em* **bold** `code`\n");
        let MdBlock::Paragraph(spans) = &blocks[0] else {
            panic!("expected paragraph, got {blocks:?}");
        };
        assert_eq!(
            spans,
            &vec![
                text("plain "),
                MdInline::Emphasis(vec![text("em")]),
                text(" "),
                MdInline::Strong(vec![text("bold")]),
                text(" "),
                MdInline::Code("code".to_string()),
            ]
        );
    }

    #[test]
    fn parses_fenced_code_block_with_language() {
        let blocks = parse_markdown("```rust\nfn main() {}\n```\n");
        assert_eq!(
            blocks,
            vec![MdBlock::Code {
                lang: Some("rust".to_string()),
                code: "fn main() {}\n".to_string(),
            }]
        );
    }

    #[test]
    fn parses_link_content_and_url() {
        let blocks = parse_markdown("see [the docs](https://example.com)\n");
        let MdBlock::Paragraph(spans) = &blocks[0] else {
            panic!("expected paragraph, got {blocks:?}");
        };
        assert_eq!(
            spans,
            &vec![
                text("see "),
                MdInline::Link {
                    content: vec![text("the docs")],
                    url: "https://example.com".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parses_nested_list_with_depth_in_order() {
        let blocks = parse_markdown("- a\n- b\n  - c\n- d\n");
        let shape: Vec<(usize, &str)> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::ListItem { depth, content, .. } => match content.first() {
                    Some(MdInline::Text(t)) => Some((*depth, t.as_str())),
                    _ => Some((*depth, "")),
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            shape,
            vec![(0, "a"), (0, "b"), (1, "c"), (0, "d")],
            "nested item keeps source order and gains depth"
        );
    }

    #[test]
    fn lookup_language_finds_known_and_rejects_unknown() {
        let registry = stoat_language::LanguageRegistry::standard();
        assert!(lookup_language(&registry, "rust").is_some(), "by name");
        assert!(lookup_language(&registry, "no-such-language-xyz").is_none());
    }

    #[test]
    fn highlights_rust_code_into_runs() {
        let registry = stoat_language::LanguageRegistry::standard();
        let theme = stoat::theme::Theme::empty();
        let code = "fn main() {}\n";
        let runs = highlight_code_runs(code, "rust", &registry, &theme);
        assert!(
            !runs.is_empty(),
            "rust captures should produce highlight runs"
        );
        assert!(
            runs.iter().all(|(r, _)| r.end <= code.len()),
            "runs stay within the code bounds"
        );
    }

    #[test]
    fn highlight_code_runs_empty_for_unknown_language() {
        let registry = stoat_language::LanguageRegistry::standard();
        let theme = stoat::theme::Theme::empty();
        assert!(highlight_code_runs("x = 1", "no-such-language-xyz", &registry, &theme).is_empty());
    }
}
