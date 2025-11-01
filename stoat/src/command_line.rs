//! Command-line mode for vim-style commands.
//!
//! Provides command parsing and execution for commands like `:cd`, `:w`, `:q`.

use std::path::PathBuf;

/// Parsed command from command-line input.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Change directory command: `:cd <path>`
    ChangeDirectory { path: PathBuf },
}

/// Error parsing command-line input.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// Empty command (just `:`)
    EmptyCommand,
    /// Unknown command name
    UnknownCommand(String),
    /// Missing required argument
    MissingArgument { command: String, expected: String },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCommand => write!(f, "Empty command"),
            Self::UnknownCommand(cmd) => write!(f, "Unknown command: :{}", cmd),
            Self::MissingArgument { command, expected } => {
                write!(f, ":{} requires {}", command, expected)
            },
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a command-line string (without the leading `:`).
///
/// # Examples
///
/// ```ignore
/// parse_command("cd /tmp") // => Ok(Command::ChangeDirectory { path: "/tmp" })
/// parse_command("cd ../foo") // => Ok(Command::ChangeDirectory { path: "../foo" })
/// parse_command("cd") // => Err(ParseError::MissingArgument)
/// ```
pub fn parse_command(input: &str) -> Result<Command, ParseError> {
    let input = input.trim();

    if input.is_empty() {
        return Err(ParseError::EmptyCommand);
    }

    let parts: Vec<&str> = input.split_whitespace().collect();
    let command_name = parts[0];

    match command_name {
        "cd" => {
            if parts.len() < 2 {
                return Err(ParseError::MissingArgument {
                    command: "cd".to_string(),
                    expected: "directory path".to_string(),
                });
            }

            // Join remaining parts to handle paths with spaces
            let path_str = parts[1..].join(" ");

            // Expand tilde if present
            let path = if path_str.starts_with('~') {
                if let Some(home) = dirs::home_dir() {
                    let remainder = &path_str[1..];
                    if remainder.is_empty() || remainder.starts_with('/') {
                        home.join(&remainder[1..])
                    } else {
                        PathBuf::from(path_str)
                    }
                } else {
                    PathBuf::from(path_str)
                }
            } else {
                PathBuf::from(path_str)
            };

            Ok(Command::ChangeDirectory { path })
        },
        _ => Err(ParseError::UnknownCommand(command_name.to_string())),
    }
}

/// Command line view component.
///
/// Renders the command line input UI at the bottom of the editor,
/// showing `:` followed by the current input text.
pub struct CommandLineView {
    /// Current input text
    pub text: String,
    /// Error message to display (if any)
    pub error: Option<String>,
}

impl CommandLineView {
    /// Create a new command line view with the given input text.
    pub fn new(text: String, error: Option<String>) -> Self {
        Self { text, error }
    }
}

impl gpui::IntoElement for CommandLineView {
    type Element = gpui::Div;

    fn into_element(self) -> Self::Element {
        use gpui::{div, px, rgb, ParentElement, Styled};

        let input_bg = if self.error.is_some() {
            rgb(0x3e1e1e) // Darker red background for errors
        } else {
            rgb(0x252526) // Normal background
        };

        let input_text = if let Some(ref error) = self.error {
            error.clone()
        } else {
            format!(":{}", self.text)
        };

        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(24.0))
            .bg(input_bg)
            .border_t_1()
            .border_color(rgb(0x3e3e42))
            .px(px(8.0))
            .flex()
            .items_center()
            .text_color(if self.error.is_some() {
                rgb(0xff6b6b) // Red text for errors
            } else {
                rgb(0xd4d4d4) // Normal text
            })
            .text_size(px(12.0))
            .child(input_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cd_with_absolute_path() {
        let result = parse_command("cd /tmp");
        assert_eq!(
            result,
            Ok(Command::ChangeDirectory {
                path: PathBuf::from("/tmp")
            })
        );
    }

    #[test]
    fn parses_cd_with_relative_path() {
        let result = parse_command("cd ../other");
        assert_eq!(
            result,
            Ok(Command::ChangeDirectory {
                path: PathBuf::from("../other")
            })
        );
    }

    #[test]
    fn parses_cd_with_tilde() {
        let result = parse_command("cd ~/projects");
        match result {
            Ok(Command::ChangeDirectory { path }) => {
                assert!(path.to_string_lossy().contains("projects"));
                assert!(!path.to_string_lossy().contains('~'));
            },
            _ => panic!("Expected ChangeDirectory command"),
        }
    }

    #[test]
    fn cd_without_path_returns_error() {
        let result = parse_command("cd");
        assert!(matches!(result, Err(ParseError::MissingArgument { .. })));
    }

    #[test]
    fn empty_command_returns_error() {
        let result = parse_command("");
        assert_eq!(result, Err(ParseError::EmptyCommand));
    }

    #[test]
    fn unknown_command_returns_error() {
        let result = parse_command("foo bar");
        assert_eq!(result, Err(ParseError::UnknownCommand("foo".to_string())));
    }
}
