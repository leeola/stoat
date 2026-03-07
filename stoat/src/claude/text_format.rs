pub enum TextBlock {
    Prose(String),
    CodeFence {
        language: Option<String>,
        content: String,
    },
}

pub fn parse_blocks(text: &str) -> Vec<TextBlock> {
    let mut blocks = Vec::new();
    let mut current_prose = String::new();
    let mut in_fence = false;
    let mut fence_lang: Option<String> = None;
    let mut fence_content = String::new();

    for line in text.lines() {
        if !in_fence && line.starts_with("```") {
            if !current_prose.is_empty() {
                blocks.push(TextBlock::Prose(current_prose.clone()));
                current_prose.clear();
            }
            let lang = line[3..].trim();
            fence_lang = if lang.is_empty() {
                None
            } else {
                Some(lang.to_string())
            };
            in_fence = true;
            fence_content.clear();
        } else if in_fence && line.starts_with("```") {
            blocks.push(TextBlock::CodeFence {
                language: fence_lang.take(),
                content: fence_content.clone(),
            });
            fence_content.clear();
            in_fence = false;
        } else if in_fence {
            if !fence_content.is_empty() {
                fence_content.push('\n');
            }
            fence_content.push_str(line);
        } else {
            if !current_prose.is_empty() {
                current_prose.push('\n');
            }
            current_prose.push_str(line);
        }
    }

    if in_fence {
        if !fence_content.is_empty() {
            blocks.push(TextBlock::Prose(fence_content));
        }
    }
    if !current_prose.is_empty() {
        blocks.push(TextBlock::Prose(current_prose));
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        let blocks = parse_blocks("hello world");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], TextBlock::Prose(t) if t == "hello world"));
    }

    #[test]
    fn code_fence_with_language() {
        let input = "before\n```rust\nfn main() {}\n```\nafter";
        let blocks = parse_blocks(input);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], TextBlock::Prose(t) if t == "before"));
        assert!(
            matches!(&blocks[1], TextBlock::CodeFence { language, content } if language.as_deref() == Some("rust") && content == "fn main() {}")
        );
        assert!(matches!(&blocks[2], TextBlock::Prose(t) if t == "after"));
    }

    #[test]
    fn code_fence_no_language() {
        let input = "```\ncode here\n```";
        let blocks = parse_blocks(input);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], TextBlock::CodeFence { language, content } if language.is_none() && content == "code here")
        );
    }

    #[test]
    fn unclosed_fence() {
        let input = "before\n```rust\nfn main() {}";
        let blocks = parse_blocks(input);
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], TextBlock::Prose(t) if t == "before"));
        assert!(matches!(&blocks[1], TextBlock::Prose(t) if t == "fn main() {}"));
    }

    #[test]
    fn multiple_fences() {
        let input = "```js\nalert(1)\n```\nmiddle\n```py\nprint(2)\n```";
        let blocks = parse_blocks(input);
        assert_eq!(blocks.len(), 3);
        assert!(
            matches!(&blocks[0], TextBlock::CodeFence { language, content } if language.as_deref() == Some("js") && content == "alert(1)")
        );
        assert!(matches!(&blocks[1], TextBlock::Prose(t) if t == "middle"));
        assert!(
            matches!(&blocks[2], TextBlock::CodeFence { language, content } if language.as_deref() == Some("py") && content == "print(2)")
        );
    }

    #[test]
    fn empty_input() {
        let blocks = parse_blocks("");
        assert!(blocks.is_empty());
    }

    #[test]
    fn multiline_prose() {
        let blocks = parse_blocks("line1\nline2\nline3");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], TextBlock::Prose(t) if t == "line1\nline2\nline3"));
    }

    #[test]
    fn multiline_code_fence() {
        let input = "```\nline1\nline2\nline3\n```";
        let blocks = parse_blocks(input);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], TextBlock::CodeFence { content, .. } if content == "line1\nline2\nline3")
        );
    }
}
