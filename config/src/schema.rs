//! Declarative schema for the settings [`crate::Settings::apply`] recognizes.
//!
//! [`settings_schema`] lists one [`SettingDef`] per setting path the parser
//! understands, with its value shape, a one-line doc, and its default. It is
//! the single source of truth a settings language server reads to offer path
//! and value completion, flag unknown settings, and show hover text, so those
//! features cannot drift ahead of the code that actually applies the settings.

/// A segment of a setting's dotted path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSeg {
    /// A fixed segment, e.g. `editor` or `scrolloff`.
    Lit(&'static str),
    /// A user-chosen segment, e.g. the `<language>` in `lsp.server.<language>`.
    /// The string is a placeholder label for display, not a fixed match.
    Wildcard(&'static str),
}

/// The value a setting accepts, driving value completion and validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueShape {
    /// `true` or `false`.
    Bool,
    /// A numeric literal.
    Number,
    /// A quoted string or bareword.
    String,
    /// One of a fixed set of barewords.
    Enum(&'static [&'static str]),
    /// An array of strings, e.g. `["-l"]`.
    StringArray,
}

/// One recognized setting, carrying its path, value shape, a one-line doc, and
/// default.
///
/// Each entry mirrors an arm of [`crate::Settings::apply`].
#[derive(Debug, Clone, Copy)]
pub struct SettingDef {
    pub path: &'static [PathSeg],
    pub shape: ValueShape,
    pub doc: &'static str,
    pub default: &'static str,
}

/// The full settings schema, one entry per setting path
/// [`crate::Settings::apply`] recognizes, in the order it matches them.
pub fn settings_schema() -> &'static [SettingDef] {
    use PathSeg::{Lit, Wildcard};

    &[
        SettingDef {
            path: &[Lit("text_proto_log")],
            shape: ValueShape::Bool,
            doc: "Enable the LSP text-protocol transcript log.",
            default: "false",
        },
        SettingDef {
            path: &[Lit("format_on_save")],
            shape: ValueShape::Bool,
            doc: "Run LSP formatting on the focused buffer before each save when \
                  the server supports it.",
            default: "false",
        },
        SettingDef {
            path: &[Lit("review"), Lit("follow")],
            shape: ValueShape::Bool,
            doc: "Whether an open review session follows the project as files, git \
                  state, and edits change.",
            default: "true",
        },
        SettingDef {
            path: &[Lit("review"), Lit("rebase_head")],
            shape: ValueShape::Bool,
            doc: "Whether a paused-rebase clean-tree diff shows the applied commit \
                  and follows each step.",
            default: "true",
        },
        SettingDef {
            path: &[Lit("review"), Lit("precompute")],
            shape: ValueShape::Bool,
            doc: "Whether the diff cache warms in the background so opening review \
                  is near-instant.",
            default: "true",
        },
        SettingDef {
            path: &[Lit("theme")],
            shape: ValueShape::String,
            doc: "Name of the active theme block, resolved against `theme NAME \
                  { ... }` blocks in the config. A theme may extend another with \
                  `theme NAME inherits PARENT { ... }`.",
            default: "built-in",
        },
        SettingDef {
            path: &[Lit("ui"), Lit("mode_badge"), Wildcard("name")],
            shape: ValueShape::String,
            doc: "Per-mode status-line badge label override, keyed by mode name.",
            default: "mode default",
        },
        SettingDef {
            path: &[Lit("mouse"), Lit("capture")],
            shape: ValueShape::Enum(&["auto", "always", "never"]),
            doc: "Mouse-capture policy at terminal startup.",
            default: "auto",
        },
        SettingDef {
            path: &[Lit("editor"), Lit("scrolloff")],
            shape: ValueShape::Number,
            doc: "Rows kept between the primary cursor and the top or bottom edge \
                  when following it.",
            default: "3",
        },
        SettingDef {
            path: &[Lit("editor"), Lit("highlight_retention")],
            shape: ValueShape::Number,
            doc: "How many hidden buffers keep their full highlight state before \
                  the least-recently-shown are evicted.",
            default: "64",
        },
        SettingDef {
            path: &[Lit("editor"), Lit("line_numbers")],
            shape: ValueShape::Enum(&["off", "absolute", "relative"]),
            doc: "How the editor gutter numbers lines (`false` means off, `true` \
                  means relative).",
            default: "relative",
        },
        SettingDef {
            path: &[Lit("editor"), Lit("minimap")],
            shape: ValueShape::Enum(&["off", "per_pane", "single"]),
            doc: "Minimap strip mode for editor panes under stoatty (off, \
                  per_pane, or single window-right strip). `false` means off, \
                  `true` means single.",
            default: "single",
        },
        SettingDef {
            path: &[Lit("editor"), Lit("wrap")],
            shape: ValueShape::Enum(&["none", "editor_width", "bounded"]),
            doc: "How editor panes soft-wrap long lines (none, editor_width, or \
                  bounded at editor.wrap_column). `false` means none, `true` \
                  means editor_width.",
            default: "editor_width",
        },
        SettingDef {
            path: &[Lit("editor"), Lit("wrap_column")],
            shape: ValueShape::Number,
            doc: "Column that bounded wrap mode wraps at, clamped to the pane \
                  text width.",
            default: "80",
        },
        SettingDef {
            path: &[Lit("ui"), Lit("inactive_dim")],
            shape: ValueShape::Number,
            doc: "Fraction an unfocused pane's colors blend toward the background \
                  (0 disables).",
            default: "0.25",
        },
        SettingDef {
            path: &[Lit("terminal"), Lit("shell")],
            shape: ValueShape::String,
            doc: "Program a terminal pane spawns as its subshell.",
            default: "$SHELL",
        },
        SettingDef {
            path: &[Lit("terminal"), Lit("args")],
            shape: ValueShape::StringArray,
            doc: "Arguments passed to the terminal pane's subshell.",
            default: "none",
        },
        SettingDef {
            path: &[Lit("lsp"), Lit("server"), Wildcard("language")],
            shape: ValueShape::StringArray,
            doc: "Per-language language-server command override, an argv keyed by \
                  language name.",
            default: "built-in",
        },
        SettingDef {
            path: &[Lit("finder"), Lit("scope"), Wildcard("name")],
            shape: ValueShape::StringArray,
            doc: "Named finder scope, a list of workspace-relative globs.",
            default: "none",
        },
        SettingDef {
            path: &[Lit("finder"), Lit("default_scope")],
            shape: ValueShape::String,
            doc: "Name of the finder scope a fresh workspace opens in.",
            default: "all",
        },
        SettingDef {
            path: &[Lit("direnv"), Lit("load")],
            shape: ValueShape::Bool,
            doc: "Whether workspaces load their direnv environment automatically.",
            default: "true",
        },
        SettingDef {
            path: &[Lit("direnv"), Lit("reload_on_cd")],
            shape: ValueShape::Bool,
            doc: "Whether changing the working directory reloads the workspace's \
                  direnv environment.",
            default: "true",
        },
        SettingDef {
            path: &[Lit("direnv"), Lit("unset_on_exit")],
            shape: ValueShape::Bool,
            doc: "Whether a direnv diff that only reverts the inherited environment \
                  is applied.",
            default: "false",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Settings};

    /// A concrete dotted path for a def, filling wildcards with a placeholder.
    fn fill_path(path: &[PathSeg]) -> String {
        path.iter()
            .map(|seg| match seg {
                PathSeg::Lit(segment) => *segment,
                PathSeg::Wildcard(_) => "sample",
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// A stcfg value literal that `apply` accepts for `shape`.
    fn sample_value(shape: ValueShape) -> String {
        match shape {
            ValueShape::Bool => "true".to_string(),
            ValueShape::Number => "1".to_string(),
            ValueShape::String => "\"x\"".to_string(),
            ValueShape::Enum(values) => values[0].to_string(),
            ValueShape::StringArray => "[\"x\"]".to_string(),
        }
    }

    #[test]
    fn every_schema_def_is_recognized_by_apply() {
        for def in settings_schema() {
            let key = fill_path(def.path);
            let value = sample_value(def.shape);
            let source = format!("on init {{ {key} = {value}; }}");

            let (config, errors) = parse(&source);
            assert!(errors.is_empty(), "`{source}` failed to parse: {errors:?}");
            let config = config.expect("parsed config");

            assert_ne!(
                Settings::from_config(&config),
                Settings::default(),
                "schema path `{key}` = `{value}` is not recognized by Settings::apply",
            );
        }
    }
}
