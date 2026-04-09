use crate::display_map::highlights::{HighlightStyle, HighlightStyleId, HighlightStyleInterner};
use ratatui::style::Color;
use std::sync::Arc;
use stoat_language::TokenStyle;

#[derive(Clone)]
pub struct SyntaxStyles {
    pub interner: Arc<HighlightStyleInterner>,
    table: Vec<HighlightStyleId>,
}

impl SyntaxStyles {
    pub fn standard() -> Self {
        let mut interner = HighlightStyleInterner::default();
        let table: Vec<HighlightStyleId> = TokenStyle::ALL
            .iter()
            .map(|ts| interner.intern(style_for(*ts)))
            .collect();
        Self {
            interner: Arc::new(interner),
            table,
        }
    }

    pub fn id(&self, ts: TokenStyle) -> HighlightStyleId {
        self.table[token_style_index(ts)]
    }
}

fn token_style_index(ts: TokenStyle) -> usize {
    TokenStyle::ALL
        .iter()
        .position(|s| *s == ts)
        .expect("TokenStyle::ALL must contain every variant")
}

fn style_for(ts: TokenStyle) -> HighlightStyle {
    let mut s = HighlightStyle::default();
    match ts {
        TokenStyle::Keyword | TokenStyle::KeywordControl => {
            s.foreground = Some(Color::Blue);
            s.bold = Some(true);
        },
        TokenStyle::String => {
            s.foreground = Some(Color::Green);
        },
        TokenStyle::StringEscape => {
            s.foreground = Some(Color::LightGreen);
            s.bold = Some(true);
        },
        TokenStyle::Comment => {
            s.foreground = Some(Color::DarkGray);
            s.italic = Some(true);
        },
        TokenStyle::CommentDoc => {
            s.foreground = Some(Color::Gray);
            s.italic = Some(true);
        },
        TokenStyle::Function | TokenStyle::FunctionMethod => {
            s.foreground = Some(Color::Yellow);
        },
        TokenStyle::FunctionSpecial => {
            s.foreground = Some(Color::LightYellow);
            s.bold = Some(true);
        },
        TokenStyle::Type | TokenStyle::TypeBuiltin | TokenStyle::TypeInterface => {
            s.foreground = Some(Color::Cyan);
        },
        TokenStyle::Constant | TokenStyle::ConstantBuiltin => {
            s.foreground = Some(Color::Magenta);
        },
        TokenStyle::Boolean | TokenStyle::Number => {
            s.foreground = Some(Color::Magenta);
        },
        TokenStyle::Operator => {
            s.foreground = Some(Color::LightCyan);
        },
        TokenStyle::PunctuationBracket | TokenStyle::PunctuationDelimiter => {
            s.foreground = Some(Color::Gray);
        },
        TokenStyle::Property => {
            s.foreground = Some(Color::LightBlue);
        },
        TokenStyle::Attribute => {
            s.foreground = Some(Color::LightMagenta);
        },
        TokenStyle::Variable => {
            s.foreground = Some(Color::White);
        },
        TokenStyle::VariableParameter => {
            s.foreground = Some(Color::White);
        },
        TokenStyle::VariableSpecial => {
            s.foreground = Some(Color::LightRed);
            s.italic = Some(true);
        },
        TokenStyle::Lifetime => {
            s.foreground = Some(Color::LightYellow);
            s.italic = Some(true);
        },
        TokenStyle::Title => {
            s.foreground = Some(Color::LightCyan);
            s.bold = Some(true);
        },
        TokenStyle::LinkText => {
            s.foreground = Some(Color::LightBlue);
        },
        TokenStyle::LinkUri => {
            s.foreground = Some(Color::Blue);
            s.underline = Some(true);
        },
        TokenStyle::Emphasis => {
            s.italic = Some(true);
        },
        TokenStyle::EmphasisStrong => {
            s.bold = Some(true);
        },
        TokenStyle::LiteralMarkup => {
            s.foreground = Some(Color::LightYellow);
        },
        TokenStyle::Strikethrough => {
            s.strikethrough = Some(true);
        },
    }
    s
}

#[cfg(test)]
mod tests {
    use super::SyntaxStyles;
    use stoat_language::TokenStyle;

    #[test]
    fn id_resolves_every_token_style() {
        let styles = SyntaxStyles::standard();
        for ts in TokenStyle::ALL {
            // Must not panic and must return a valid id (interned style exists).
            let id = styles.id(*ts);
            let _style = &styles.interner[id];
        }
    }

    #[test]
    fn distinct_token_styles_get_distinct_ids_when_styles_differ() {
        let styles = SyntaxStyles::standard();
        let kw = styles.id(TokenStyle::Keyword);
        let string = styles.id(TokenStyle::String);
        assert_ne!(
            styles.interner[kw], styles.interner[string],
            "Keyword and String should produce visually distinct styles"
        );
    }
}
