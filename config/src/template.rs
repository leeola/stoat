use std::{borrow::Cow, mem};

/// A string of a `command` item with `{name}` placeholders, as
/// `shell("gh pr checkout {pr}")` writes one.
///
/// A placeholder is `{`, an identifier of ASCII letters, digits, and `_` that
/// starts with a letter or `_`, and `}`. The text `{{name}}` writes the
/// literal `{name}`, and `${` passes through as text, so the shell's own
/// `${HOME}` stays. Every other brace is plain text, so `awk '{print $1}'` and
/// `find -exec {}` take no escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<TemplatePart>,
}

/// A run of text or a placeholder of a [`Template`], in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplatePart {
    Text(String),
    Placeholder(String),
}

/// The quote state of a shell line at a char, for [`Template::render_shell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    Outside,
    Double,
    Single,
}

impl Template {
    /// Read `source` into its text and its placeholders.
    ///
    /// Every source reads, because a brace that names no placeholder is text.
    pub fn parse(source: &str) -> Template {
        let bytes = source.as_bytes();
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut at = 0;

        while at < bytes.len() {
            match bytes[at] {
                b'$' if bytes[at..].starts_with(b"${") => {
                    text.push_str("${");
                    at += 2;
                },
                b'{' => {
                    if bytes[at..].starts_with(b"{{")
                        && let Some(end) = identifier_end(bytes, at + 2)
                        && bytes[end..].starts_with(b"}}")
                    {
                        text.push('{');
                        text.push_str(&source[at + 2..end]);
                        text.push('}');
                        at = end + 2;
                    } else if let Some(end) = identifier_end(bytes, at + 1)
                        && bytes[end..].starts_with(b"}")
                    {
                        if !text.is_empty() {
                            parts.push(TemplatePart::Text(mem::take(&mut text)));
                        }
                        parts.push(TemplatePart::Placeholder(source[at + 1..end].to_owned()));
                        at = end + 1;
                    } else {
                        text.push('{');
                        at += 1;
                    }
                },
                _ => {
                    let ch = source[at..].chars().next().expect("a char starts here");
                    text.push(ch);
                    at += ch.len_utf8();
                },
            }
        }

        if !text.is_empty() {
            parts.push(TemplatePart::Text(text));
        }
        Template { parts }
    }

    /// List the names of the placeholders in source order, a name once for
    /// each place it stands.
    pub fn placeholders(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            TemplatePart::Placeholder(name) => Some(name.as_str()),
            TemplatePart::Text(_) => None,
        })
    }

    /// Write the template with the value of each placeholder as it is.
    ///
    /// A name with no value writes the empty string.
    pub fn render(&self, value: impl Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                TemplatePart::Text(text) => out.push_str(text),
                TemplatePart::Placeholder(name) => out.push_str(&value(name).unwrap_or_default()),
            }
        }
        out
    }

    /// Write the template as a shell line, with each value escaped for the
    /// quotes around its placeholder.
    ///
    /// A value outside quotes becomes one word, so a title with spaces stays
    /// one argument. The word is bare when each char is an ASCII letter, a
    /// digit, or one of `_@%+=:,./-`. Any other value goes in single quotes
    /// with each `'` written `'\''`, and the empty value is `''`.
    ///
    /// A value inside the template's double quotes takes a backslash before
    /// each `\`, `"`, `$`, and backquote. A value inside its single quotes
    /// writes each `'` as `'\''`. A name with no value writes the empty
    /// string.
    ///
    /// A backslash that ends the text before a placeholder stands for itself,
    /// so it takes no char of the value.
    ///
    /// The quote state follows only backslashes and quotes, not shell comments
    /// or here-documents. A value in one of those, or after a quote char in
    /// one, gets the wrong escape.
    pub fn render_shell(&self, value: impl Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        let mut quote = Quote::Outside;
        let mut escaped = false;

        for part in &self.parts {
            match part {
                TemplatePart::Text(text) => {
                    for ch in text.chars() {
                        (quote, escaped) = shell_step(quote, escaped, ch);
                    }
                    out.push_str(text);
                },
                TemplatePart::Placeholder(name) => {
                    // A lone backslash at the end of the text escapes the
                    // first char of the value. When that char opens the quotes
                    // of the value, the word breaks, so a second backslash
                    // pairs with the first.
                    if escaped {
                        out.push('\\');
                        escaped = false;
                    }

                    let value = value(name).unwrap_or_default();
                    match quote {
                        Quote::Outside => out.push_str(&shell_quote(&value)),
                        Quote::Double => {
                            for ch in value.chars() {
                                if matches!(ch, '\\' | '"' | '$' | '`') {
                                    out.push('\\');
                                }
                                out.push(ch);
                            }
                        },
                        Quote::Single => out.push_str(&value.replace('\'', r"'\''")),
                    }
                },
            }
        }
        out
    }
}

/// The end of the identifier that starts at `start` in `bytes`, or `None`
/// when no identifier starts there.
fn identifier_end(bytes: &[u8], start: usize) -> Option<usize> {
    let first = *bytes.get(start)?;
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return None;
    }

    let len = bytes[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    Some(start + len)
}

/// The quote state of a shell line after `ch`, from `quote` and whether the
/// char before `ch` was an escaping backslash.
///
/// Outside quotes a backslash takes the next char with it, `'` opens single
/// quotes, and `"` opens double quotes. Inside double quotes a backslash
/// takes the next char and `"` closes. Inside single quotes only `'` closes.
fn shell_step(quote: Quote, escaped: bool, ch: char) -> (Quote, bool) {
    if escaped {
        return (quote, false);
    }
    match (quote, ch) {
        (Quote::Outside, '\\') | (Quote::Double, '\\') => (quote, true),
        (Quote::Outside, '\'') => (Quote::Single, false),
        (Quote::Outside, '"') => (Quote::Double, false),
        (Quote::Double, '"') | (Quote::Single, '\'') => (Quote::Outside, false),
        _ => (quote, false),
    }
}

/// `value` as one shell word, by the rule that [`Template::render_shell`]
/// states for a value outside quotes.
fn shell_quote(value: &str) -> Cow<'_, str> {
    if value.is_empty() {
        return Cow::Borrowed("''");
    }

    let bare = value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "_@%+=:,./-".contains(ch));
    match bare {
        true => Cow::Borrowed(value),
        false => Cow::Owned(format!("'{}'", value.replace('\'', r"'\''"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{Template, TemplatePart};

    /// A value lookup that gives `value` for the placeholder `m` and no value
    /// for any other name.
    fn m(value: &str) -> impl Fn(&str) -> Option<String> {
        move |name| (name == "m").then(|| value.to_owned())
    }

    /// `source` as a shell line, with `value` for each placeholder `m`.
    fn shell(source: &str, value: &str) -> String {
        Template::parse(source).render_shell(m(value))
    }

    #[test]
    fn a_template_splits_at_its_placeholders() {
        assert_eq!(
            Template::parse("gh pr checkout {pr}").parts,
            [
                TemplatePart::Text("gh pr checkout ".to_owned()),
                TemplatePart::Placeholder("pr".to_owned()),
            ],
        );
    }

    /// Braces that name no placeholder stay text, and `{{name}}` writes the
    /// literal `{name}`.
    #[test]
    fn braces_that_name_no_placeholder_stay_text() {
        let template = Template::parse("awk '{print $1}' ${HOME} {} {a b} {1x} {{name}}");
        assert_eq!(
            (
                template.placeholders().count(),
                template.render(|_| Some("value".to_owned())),
            ),
            (
                0,
                "awk '{print $1}' ${HOME} {} {a b} {1x} {name}".to_owned()
            ),
        );
    }

    /// A value outside quotes stays one shell word, bare when its chars are
    /// safe and in single quotes otherwise.
    #[test]
    fn a_value_outside_quotes_is_one_shell_word() {
        let values = [
            "412",
            "v1.2_rc+4@host%x=y:z,a/b-c",
            "fix: a thing",
            "",
            "it's",
        ];
        assert_eq!(
            values.map(|value| shell("gh pr create --title {m}", value)),
            [
                "gh pr create --title 412",
                "gh pr create --title v1.2_rc+4@host%x=y:z,a/b-c",
                "gh pr create --title 'fix: a thing'",
                "gh pr create --title ''",
                r"gh pr create --title 'it'\''s'",
            ],
        );
    }

    /// A value inside double quotes escapes the four chars that they expand,
    /// and takes no quote of its own.
    #[test]
    fn a_value_inside_double_quotes_escapes_what_they_expand() {
        assert_eq!(
            shell(r#"echo "{m}""#, r#"a"b$c\d`e"#),
            r#"echo "a\"b\$c\\d\`e""#,
        );
    }

    #[test]
    fn a_value_inside_single_quotes_escapes_only_the_quote() {
        assert_eq!(shell("echo '{m}'", "it's $x"), r"echo 'it'\''s $x'");
    }

    /// A backslash before a quote in the template escapes it, so the value
    /// after it stands outside quotes.
    #[test]
    fn an_escaped_quote_in_the_template_opens_no_quote() {
        assert_eq!(shell(r#"echo \"{m}"#, "a b"), r#"echo \"'a b'"#);
    }

    #[test]
    fn an_escaped_quote_inside_double_quotes_closes_no_quote() {
        assert_eq!(shell(r#"echo "\"{m}""#, "a b"), r#"echo "\"a b""#);
    }

    #[test]
    fn a_closed_quote_leaves_the_value_outside_quotes() {
        assert_eq!(
            [
                shell("printf '%s' {m}", "a b"),
                shell(r#"echo "x" {m}"#, "a b"),
            ],
            ["printf '%s' 'a b'", r#"echo "x" 'a b'"#],
        );
    }

    /// A backslash right before a placeholder stands for itself. The value
    /// keeps its quotes, and the text after it reads with no escape.
    #[test]
    fn a_backslash_before_a_placeholder_stands_for_itself() {
        assert_eq!(
            [
                shell(r"echo \{m}", "a b"),
                shell(r#"echo "\{m}" "{m}""#, "a b"),
            ],
            [r"echo \\'a b'", r#"echo "\\a b" "a b""#],
        );
    }

    /// `render` writes each value as it is, and a name with no value writes
    /// the empty string.
    #[test]
    fn render_writes_each_value_as_it_is() {
        let template = Template::parse("#{number} {title}");
        let number = |name: &str| (name == "number").then(|| "412".to_owned());
        let title = |name: &str| {
            number(name).or_else(|| (name == "title").then(|| r#"Fix "it""#.to_owned()))
        };
        assert_eq!(
            [template.render(title), template.render(number)],
            [r#"#412 Fix "it""#, "#412 "],
        );
    }

    #[test]
    fn placeholders_lists_each_name_in_source_order() {
        let template = Template::parse("{b} and {a_1}, then {b} again");
        assert_eq!(
            template.placeholders().collect::<Vec<_>>(),
            ["b", "a_1", "b"]
        );
    }
}
