use crate::syntax::{HighlightMap, SyntaxTheme};
use gpui::{FontStyle, FontWeight, Hsla};
use stoat_lsp::response::{HoverBlock, HoverBlockKind};
use stoat_text::{Language, Parser};

const PROSE_COLOR: Hsla = Hsla {
    h: 0.0,
    s: 0.0,
    l: 0.65,
    a: 1.0,
};

#[derive(Clone, Debug)]
pub struct StyledSpan {
    pub text: String,
    pub color: Hsla,
    pub font_weight: FontWeight,
    pub font_style: FontStyle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SectionKind {
    Prose,
    Code,
}

#[derive(Clone, Debug)]
pub struct HoverSection {
    pub kind: SectionKind,
    pub spans: Vec<StyledSpan>,
}

#[derive(Clone, Debug, Default)]
pub struct HoverState {
    pub blocks: Vec<HoverBlock>,
    pub sections: Vec<HoverSection>,
    pub visible: bool,
}

impl HoverState {
    pub fn dismiss(&mut self) {
        self.visible = false;
        self.blocks.clear();
        self.sections.clear();
    }

    pub fn set_blocks(&mut self, blocks: Vec<HoverBlock>, theme: &SyntaxTheme) {
        self.sections = format_hover_blocks(&blocks, theme);
        self.blocks = blocks;
        self.visible = true;
    }
}

pub fn format_hover_blocks(blocks: &[HoverBlock], theme: &SyntaxTheme) -> Vec<HoverSection> {
    let mut sections = Vec::new();
    for block in blocks {
        match &block.kind {
            HoverBlockKind::Markdown => {
                sections.extend(render_markdown(&block.text, theme));
            },
            HoverBlockKind::Code { language } => {
                sections.push(HoverSection {
                    kind: SectionKind::Code,
                    spans: highlight_code(&block.text, language, theme),
                });
            },
            HoverBlockKind::PlainText => {
                sections.push(HoverSection {
                    kind: SectionKind::Prose,
                    spans: vec![StyledSpan {
                        text: block.text.clone(),
                        color: PROSE_COLOR,
                        font_weight: FontWeight::NORMAL,
                        font_style: FontStyle::Normal,
                    }],
                });
            },
        }
    }
    sections
}

pub fn render_markdown(text: &str, theme: &SyntaxTheme) -> Vec<HoverSection> {
    // tree-sitter-md requires trailing newline for correct block parsing
    let normalized = if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    };

    let mut parser = match Parser::new(Language::Markdown) {
        Ok(p) => p,
        Err(_) => return vec![fallback_prose_section(text)],
    };
    if parser.parse(&normalized).is_err() {
        return vec![fallback_prose_section(text)];
    }
    let block_tree = match parser.tree() {
        Some(t) => t,
        None => return vec![fallback_prose_section(text)],
    };

    let mut sections = Vec::new();
    walk_block_children(
        block_tree.root_node(),
        &normalized,
        &parser,
        theme,
        &mut sections,
    );
    sections
}

fn walk_block_children(
    node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    parser: &Parser,
    theme: &SyntaxTheme,
    sections: &mut Vec<HoverSection>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        process_block_node(child, text, parser, theme, sections);
    }
}

fn process_block_node(
    node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    parser: &Parser,
    theme: &SyntaxTheme,
    sections: &mut Vec<HoverSection>,
) {
    match node.kind() {
        "document" | "section" => {
            walk_block_children(node, text, parser, theme, sections);
        },
        "atx_heading" | "setext_heading" => {
            let heading_text = extract_heading_text(node, text);
            if !heading_text.is_empty() {
                let heading_color = theme
                    .highlights
                    .iter()
                    .find(|(name, _)| name == "markup.heading")
                    .and_then(|(_, s)| s.color)
                    .unwrap_or(PROSE_COLOR);
                sections.push(HoverSection {
                    kind: SectionKind::Prose,
                    spans: vec![StyledSpan {
                        text: heading_text,
                        color: heading_color,
                        font_weight: FontWeight::BOLD,
                        font_style: FontStyle::Normal,
                    }],
                });
            }
        },
        "fenced_code_block" => {
            let (lang, content) = extract_code_fence(node, text);
            sections.push(HoverSection {
                kind: SectionKind::Code,
                spans: highlight_code(&content, &lang, theme),
            });
        },
        "paragraph" => {
            let spans = render_block_inline_content(node, text, parser, theme);
            if !spans.is_empty() {
                sections.push(HoverSection {
                    kind: SectionKind::Prose,
                    spans,
                });
            }
        },
        "list" | "block_quote" | "list_item" => {
            walk_block_children(node, text, parser, theme, sections);
        },
        "thematic_break" => {},
        _ => {
            let node_text = &text[node.start_byte()..node.end_byte()];
            let trimmed = node_text.trim();
            if !trimmed.is_empty() {
                sections.push(HoverSection {
                    kind: SectionKind::Prose,
                    spans: vec![default_span(trimmed, PROSE_COLOR)],
                });
            }
        },
    }
}

fn extract_heading_text(node: stoat_text::tree_sitter::Node<'_>, text: &str) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "inline" || child.kind() == "paragraph" {
            return text[child.start_byte()..child.end_byte()]
                .trim()
                .to_string();
        }
    }
    String::new()
}

fn extract_code_fence(node: stoat_text::tree_sitter::Node<'_>, text: &str) -> (String, String) {
    let mut lang = String::new();
    let mut content = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "info_string" => {
                lang = text[child.start_byte()..child.end_byte()]
                    .trim()
                    .to_string();
            },
            "code_fence_content" => {
                content = text[child.start_byte()..child.end_byte()].to_string();
                if content.ends_with('\n') {
                    content.pop();
                }
            },
            _ => {},
        }
    }
    (lang, content)
}

fn render_block_inline_content(
    block_node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    parser: &Parser,
    theme: &SyntaxTheme,
) -> Vec<StyledSpan> {
    let mut cursor = block_node.walk();
    for child in block_node.children(&mut cursor) {
        if child.kind() == "inline" {
            let range = child.start_byte()..child.end_byte();
            if let Some(inline_tree) = parser.inline_tree() {
                return render_inline_in_range(inline_tree, range, text, theme);
            }
            let raw = &text[child.start_byte()..child.end_byte()];
            if !raw.is_empty() {
                return vec![default_span(raw, PROSE_COLOR)];
            }
        }
    }
    Vec::new()
}

fn render_inline_in_range(
    inline_tree: &stoat_text::tree_sitter::Tree,
    range: std::ops::Range<usize>,
    text: &str,
    theme: &SyntaxTheme,
) -> Vec<StyledSpan> {
    let mut spans = Vec::new();
    let root = inline_tree.root_node();

    // The inline tree root covers all inline ranges; filter to our range
    if root.start_byte() >= range.start && root.end_byte() <= range.end {
        emit_gaps_and_recurse(
            root,
            text,
            &mut spans,
            PROSE_COLOR,
            FontWeight::NORMAL,
            FontStyle::Normal,
            theme,
        );
    } else {
        let mut pos = range.start;
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.end_byte() <= range.start {
                continue;
            }
            if child.start_byte() >= range.end {
                break;
            }

            let gap_start = pos.max(range.start);
            let gap_end = child.start_byte().min(range.end);
            if gap_end > gap_start {
                let gap = &text[gap_start..gap_end];
                if !gap.is_empty() {
                    spans.push(StyledSpan {
                        text: gap.to_string(),
                        color: PROSE_COLOR,
                        font_weight: FontWeight::NORMAL,
                        font_style: FontStyle::Normal,
                    });
                }
            }

            render_inline_content(
                child,
                text,
                &mut spans,
                PROSE_COLOR,
                FontWeight::NORMAL,
                FontStyle::Normal,
                theme,
            );
            pos = child.end_byte();
        }

        let trail_start = pos.max(range.start);
        if trail_start < range.end {
            let gap = &text[trail_start..range.end];
            if !gap.is_empty() {
                spans.push(StyledSpan {
                    text: gap.to_string(),
                    color: PROSE_COLOR,
                    font_weight: FontWeight::NORMAL,
                    font_style: FontStyle::Normal,
                });
            }
        }
    }

    spans
}

fn render_inline_content(
    node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    spans: &mut Vec<StyledSpan>,
    color: Hsla,
    weight: FontWeight,
    style: FontStyle,
    theme: &SyntaxTheme,
) {
    match node.kind() {
        "emphasis" => {
            emit_gaps_and_recurse(node, text, spans, color, weight, FontStyle::Italic, theme);
        },
        "strong_emphasis" => {
            emit_gaps_and_recurse(node, text, spans, color, FontWeight::BOLD, style, theme);
        },
        "code_span" => {
            render_code_span(node, text, spans, color, weight, style, theme);
        },
        "inline_link" | "full_reference_link" => {
            render_link_node(node, text, spans, weight, style, theme);
        },
        "shortcut_link" | "collapsed_reference_link" => {
            render_link_node(node, text, spans, weight, style, theme);
        },
        "image" => {
            render_image_node(node, text, spans, weight, style, theme);
        },
        _ => {
            if node.child_count() > 0 {
                emit_gaps_and_recurse(node, text, spans, color, weight, style, theme);
            } else {
                let node_text = &text[node.start_byte()..node.end_byte()];
                if !node_text.is_empty() {
                    spans.push(StyledSpan {
                        text: node_text.to_string(),
                        color,
                        font_weight: weight,
                        font_style: style,
                    });
                }
            }
        },
    }
}

fn emit_gaps_and_recurse(
    node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    spans: &mut Vec<StyledSpan>,
    color: Hsla,
    weight: FontWeight,
    style: FontStyle,
    theme: &SyntaxTheme,
) {
    let mut pos = node.start_byte();
    let end = node.end_byte();
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.start_byte() > pos {
            let gap = &text[pos..child.start_byte()];
            if !gap.is_empty() {
                spans.push(StyledSpan {
                    text: gap.to_string(),
                    color,
                    font_weight: weight,
                    font_style: style,
                });
            }
        }

        match child.kind() {
            "emphasis_delimiter" | "code_span_delimiter" => {},
            _ => {
                render_inline_content(child, text, spans, color, weight, style, theme);
            },
        }

        pos = child.end_byte();
    }

    if pos < end {
        let gap = &text[pos..end];
        if !gap.is_empty() {
            spans.push(StyledSpan {
                text: gap.to_string(),
                color,
                font_weight: weight,
                font_style: style,
            });
        }
    }
}

fn render_code_span(
    node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    spans: &mut Vec<StyledSpan>,
    color: Hsla,
    weight: FontWeight,
    style: FontStyle,
    theme: &SyntaxTheme,
) {
    let count = node.child_count();
    if count < 2 {
        return;
    }
    let first = node.child(0).unwrap();
    let last = node.child(count - 1).unwrap();
    let content_start = first.end_byte();
    let content_end = last.start_byte();
    if content_start >= content_end {
        return;
    }

    let mut code_text = &text[content_start..content_end];
    if code_text.len() >= 2 && code_text.starts_with(' ') && code_text.ends_with(' ') {
        code_text = &code_text[1..code_text.len() - 1];
    }

    let code_color = theme
        .highlights
        .iter()
        .find(|(name, _)| name == "markup.code")
        .and_then(|(_, s)| s.color)
        .unwrap_or(color);

    if !code_text.is_empty() {
        spans.push(StyledSpan {
            text: code_text.to_string(),
            color: code_color,
            font_weight: weight,
            font_style: style,
        });
    }
}

fn render_link_node(
    node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    spans: &mut Vec<StyledSpan>,
    weight: FontWeight,
    style: FontStyle,
    theme: &SyntaxTheme,
) {
    let link_color = get_link_color(theme);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "link_text" {
            let link_text = &text[child.start_byte()..child.end_byte()];
            if !link_text.is_empty() {
                spans.push(StyledSpan {
                    text: link_text.to_string(),
                    color: link_color,
                    font_weight: weight,
                    font_style: style,
                });
            }
        }
    }
}

fn render_image_node(
    node: stoat_text::tree_sitter::Node<'_>,
    text: &str,
    spans: &mut Vec<StyledSpan>,
    weight: FontWeight,
    style: FontStyle,
    theme: &SyntaxTheme,
) {
    let link_color = get_link_color(theme);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "image_description" {
            let desc = &text[child.start_byte()..child.end_byte()];
            if !desc.is_empty() {
                spans.push(StyledSpan {
                    text: desc.to_string(),
                    color: link_color,
                    font_weight: weight,
                    font_style: style,
                });
            }
        }
    }
}

fn get_link_color(theme: &SyntaxTheme) -> Hsla {
    theme
        .highlights
        .iter()
        .find(|(name, _)| name == "text.reference")
        .and_then(|(_, s)| s.color)
        .unwrap_or(PROSE_COLOR)
}

pub fn highlight_code(text: &str, language_name: &str, theme: &SyntaxTheme) -> Vec<StyledSpan> {
    let default_color = theme.default_text_color;

    let language = Language::from_name(language_name);
    let mut parser = match Parser::new(language) {
        Ok(p) => p,
        Err(_) => return vec![default_span(text, default_color)],
    };
    if parser.parse(text).is_err() {
        return vec![default_span(text, default_color)];
    }
    let tree = match parser.tree() {
        Some(t) => t,
        None => return vec![default_span(text, default_color)],
    };
    let query = match parser.highlight_query() {
        Some(q) => q,
        None => return vec![default_span(text, default_color)],
    };

    let captures = query.captures(tree, text.as_bytes(), 0..text.len());
    let highlight_map = HighlightMap::new(theme, query.capture_names());

    let mut spans = Vec::new();
    let mut pos = 0;

    for cap in &captures {
        let start = cap.byte_range.start;
        let end = cap.byte_range.end;
        if start < pos {
            continue;
        }
        if start > pos {
            spans.push(default_span(&text[pos..start], default_color));
        }
        let hid = highlight_map.get(cap.capture_index);
        let style = hid.style(theme);
        let color = style.and_then(|s| s.color).unwrap_or(default_color);
        let font_weight = style
            .and_then(|s| s.font_weight)
            .unwrap_or(FontWeight::NORMAL);
        let font_style = style
            .and_then(|s| s.font_style)
            .unwrap_or(FontStyle::Normal);
        spans.push(StyledSpan {
            text: text[start..end].to_string(),
            color,
            font_weight,
            font_style,
        });
        pos = end;
    }

    if pos < text.len() {
        spans.push(default_span(&text[pos..], default_color));
    }

    if spans.is_empty() {
        spans.push(default_span(text, default_color));
    }

    spans
}

fn default_span(text: &str, color: Hsla) -> StyledSpan {
    StyledSpan {
        text: text.to_string(),
        color,
        font_weight: FontWeight::NORMAL,
        font_style: FontStyle::Normal,
    }
}

fn fallback_prose_section(text: &str) -> HoverSection {
    HoverSection {
        kind: SectionKind::Prose,
        spans: vec![default_span(text, PROSE_COLOR)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> SyntaxTheme {
        SyntaxTheme::monokai_dark()
    }

    fn prose_spans(text: &str, theme: &SyntaxTheme) -> Vec<StyledSpan> {
        let sections = render_markdown(text, theme);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].kind, SectionKind::Prose);
        sections.into_iter().next().unwrap().spans
    }

    #[test]
    fn format_plain_text() {
        let blocks = vec![HoverBlock {
            text: "hello world".into(),
            kind: HoverBlockKind::PlainText,
        }];
        let sections = format_hover_blocks(&blocks, &theme());
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].kind, SectionKind::Prose);
        assert_eq!(sections[0].spans.len(), 1);
        assert_eq!(sections[0].spans[0].text, "hello world");
    }

    #[test]
    fn format_code_block_rust() {
        let blocks = vec![HoverBlock {
            text: "fn main() {}".into(),
            kind: HoverBlockKind::Code {
                language: "rust".into(),
            },
        }];
        let sections = format_hover_blocks(&blocks, &theme());
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].kind, SectionKind::Code);
        assert!(sections[0].spans.len() > 1);
        let full: String = sections[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(full, "fn main() {}");
        let fn_span = sections[0]
            .spans
            .iter()
            .find(|s| s.text == "fn")
            .expect("should have fn span");
        assert_ne!(fn_span.color, theme().default_text_color);
    }

    #[test]
    fn format_markdown_with_fence() {
        let md = "some docs\n```rust\nfn foo() {}\n```\nmore text";
        let blocks = vec![HoverBlock {
            text: md.into(),
            kind: HoverBlockKind::Markdown,
        }];
        let sections = format_hover_blocks(&blocks, &theme());
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].kind, SectionKind::Prose);
        assert_eq!(sections[1].kind, SectionKind::Code);
        assert_eq!(sections[2].kind, SectionKind::Prose);
        let code_text: String = sections[1].spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(code_text, "fn foo() {}");
    }

    #[test]
    fn format_inline_bold() {
        let t = theme();
        let spans = prose_spans("hello **bold** world", &t);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "hello ");
        assert_eq!(spans[1].text, "bold");
        assert_eq!(spans[1].font_weight, FontWeight::BOLD);
        assert_eq!(spans[2].text, " world");
    }

    #[test]
    fn format_inline_code() {
        let t = theme();
        let spans = prose_spans("use `Option<T>` here", &t);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "use ");
        assert_eq!(spans[1].text, "Option<T>");
        assert_ne!(spans[1].color, PROSE_COLOR);
        assert_eq!(spans[2].text, " here");
    }

    #[test]
    fn format_heading() {
        let t = theme();
        let sections = render_markdown("# Title", &t);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].spans.len(), 1);
        assert_eq!(sections[0].spans[0].text, "Title");
        assert_eq!(sections[0].spans[0].font_weight, FontWeight::BOLD);
    }

    #[test]
    fn format_inline_link() {
        let t = theme();
        let spans = prose_spans("see [docs](https://example.com) here", &t);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "see ");
        assert_eq!(spans[1].text, "docs");
        assert_ne!(spans[1].color, PROSE_COLOR);
        assert_eq!(spans[2].text, " here");
    }

    #[test]
    fn format_reference_link() {
        let t = theme();
        let spans = prose_spans("returns [`Option`]", &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "returns ");
        assert_eq!(spans[1].text, "`Option`");
        assert_ne!(spans[1].color, PROSE_COLOR);
    }

    #[test]
    fn highlight_unknown_language() {
        let t = theme();
        let spans = highlight_code("hello world", "brainfuck", &t);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello world");
        assert_eq!(spans[0].color, t.default_text_color);
    }

    #[test]
    fn format_inline_italic() {
        let t = theme();
        let spans = prose_spans("hello *italic* world", &t);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "hello ");
        assert_eq!(spans[1].text, "italic");
        assert_eq!(spans[1].font_style, FontStyle::Italic);
        assert_eq!(spans[2].text, " world");
    }

    #[test]
    fn format_nested_bold_in_italic() {
        let t = theme();
        let spans = prose_spans("*hello **bold** world*", &t);
        assert!(spans.len() >= 3);
        let bold_span = spans.iter().find(|s| s.text == "bold").unwrap();
        assert_eq!(bold_span.font_weight, FontWeight::BOLD);
        assert_eq!(bold_span.font_style, FontStyle::Italic);
    }

    #[test]
    fn format_multi_paragraph_inline() {
        let t = theme();
        let md = "Returns the `String` length.\n\nSee also [`char`].";
        let sections = render_markdown(md, &t);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].kind, SectionKind::Prose);
        assert_eq!(sections[1].kind, SectionKind::Prose);

        let p1 = &sections[0].spans;
        let p1_text: String = p1.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(p1_text, "Returns the String length.");
        assert_eq!(p1[0].text, "Returns the ");
        assert_eq!(p1[0].color, PROSE_COLOR);
        let code_span = p1.iter().find(|s| s.text == "String").unwrap();
        assert_ne!(code_span.color, PROSE_COLOR);

        let p2 = &sections[1].spans;
        let p2_text: String = p2.iter().map(|s| s.text.as_str()).collect();
        assert!(p2_text.contains("char"));
        assert!(p2_text.contains("See also"));
    }

    #[test]
    fn format_rust_hover_content() {
        let t = theme();
        let md = "```rust\npub fn len(&self) -> usize\n```\n\nReturns the length of this `String`.";
        let sections = render_markdown(md, &t);
        assert_eq!(sections[0].kind, SectionKind::Code);
        let code_text: String = sections[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(code_text, "pub fn len(&self) -> usize");

        let prose = sections
            .iter()
            .find(|s| s.kind == SectionKind::Prose)
            .unwrap();
        let prose_text: String = prose.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(prose_text.contains("Returns the length of this "));
        let code_span = prose.spans.iter().find(|s| s.text == "String").unwrap();
        assert_ne!(code_span.color, PROSE_COLOR);
    }

    #[test]
    fn format_bare_reference_link() {
        let t = theme();
        let spans = prose_spans("see [str] here", &t);
        let full: String = spans.iter().map(|s| s.text.as_str()).collect();
        // tree-sitter-md-inline parses [str] as a shortcut_link, stripping brackets
        assert_eq!(full, "see str here");
        let link_span = spans.iter().find(|s| s.text == "str").unwrap();
        assert_ne!(link_span.color, PROSE_COLOR);
    }
}
