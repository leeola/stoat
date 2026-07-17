use snafu::{OptionExt, Snafu};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    String,
    Number,
    Bool,
}

impl fmt::Display for ParamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ParamKind::String => "string",
            ParamKind::Number => "number",
            ParamKind::Bool => "bool",
        };
        f.write_str(s)
    }
}

/// Where an interactive value picker sources candidates for an argument.
///
/// This is orthogonal to [`ParamKind`]. A path argument still parses as a
/// [`ParamKind::String`] regardless of source. The source only decides what an
/// interactive picker lists beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    /// No candidate list. The argument is typed freely.
    None,
    /// Workspace files.
    Files,
    /// Currently-open buffers.
    Buffers,
    /// Workspace directories, derived from the file walk. A directory that
    /// contains no files never appears.
    Directories,
    /// Names of the configured theme blocks, for the theme switcher.
    Themes,
}

pub struct ParamDef {
    pub name: &'static str,
    pub kind: ParamKind,
    /// Where an interactive picker sources candidate values for this argument,
    /// orthogonal to [`kind`](Self::kind)'s parse type.
    pub value_source: ValueSource,
    pub required: bool,
    pub description: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Number(f64),
    Bool(bool),
}

impl ParamValue {
    pub fn parse(kind: ParamKind, input: &str) -> Result<ParamValue, ParamError> {
        match kind {
            ParamKind::String => Ok(ParamValue::String(input.to_string())),
            ParamKind::Number => {
                input
                    .parse::<f64>()
                    .ok()
                    .map(ParamValue::Number)
                    .context(ParseFailureSnafu {
                        expected: ParamKind::Number,
                        input: input.to_string(),
                    })
            },
            ParamKind::Bool => match input.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" | "on" => Ok(ParamValue::Bool(true)),
                "false" | "no" | "0" | "off" => Ok(ParamValue::Bool(false)),
                _ => ParseFailureSnafu {
                    expected: ParamKind::Bool,
                    input: input.to_string(),
                }
                .fail(),
            },
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            ParamValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            ParamValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ParamValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Snafu)]
#[snafu(visibility(pub))]
pub enum ParamError {
    #[snafu(display("missing required parameter `{name}`"))]
    Missing {
        name: &'static str,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("parameter `{name}` expects {expected}"))]
    WrongKind {
        name: &'static str,
        expected: ParamKind,
        #[snafu(implicit)]
        location: snafu::Location,
    },
    #[snafu(display("cannot parse `{input}` as {expected}"))]
    ParseFailure {
        expected: ParamKind,
        input: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_passthrough() {
        assert_eq!(
            ParamValue::parse(ParamKind::String, "hello"),
            Ok(ParamValue::String("hello".into()))
        );
        assert_eq!(
            ParamValue::parse(ParamKind::String, ""),
            Ok(ParamValue::String(String::new()))
        );
    }

    #[test]
    fn parse_number_valid() {
        assert_eq!(
            ParamValue::parse(ParamKind::Number, "42"),
            Ok(ParamValue::Number(42.0))
        );
        assert_eq!(
            ParamValue::parse(ParamKind::Number, "-2.5"),
            Ok(ParamValue::Number(-2.5))
        );
    }

    #[test]
    fn parse_number_invalid() {
        let result = ParamValue::parse(ParamKind::Number, "abc");
        assert!(matches!(result, Err(ParamError::ParseFailure { .. })));
    }

    #[test]
    fn parse_bool_aliases() {
        for s in ["true", "True", "TRUE", "yes", "1", "on"] {
            assert_eq!(
                ParamValue::parse(ParamKind::Bool, s),
                Ok(ParamValue::Bool(true)),
                "{s}"
            );
        }
        for s in ["false", "False", "no", "0", "off"] {
            assert_eq!(
                ParamValue::parse(ParamKind::Bool, s),
                Ok(ParamValue::Bool(false)),
                "{s}"
            );
        }
    }

    #[test]
    fn parse_bool_invalid() {
        assert!(matches!(
            ParamValue::parse(ParamKind::Bool, "maybe"),
            Err(ParamError::ParseFailure { .. })
        ));
    }

    #[test]
    fn accessors() {
        assert_eq!(ParamValue::String("x".into()).as_string(), Some("x"));
        assert_eq!(ParamValue::Number(1.5).as_number(), Some(1.5));
        assert_eq!(ParamValue::Bool(true).as_bool(), Some(true));
        assert_eq!(ParamValue::Number(1.0).as_string(), None);
    }
}
