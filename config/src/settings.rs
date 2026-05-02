//! Typed view of stcfg settings, with a merge operator so CLI/env flags
//! can override values loaded from config files.
//!
//! Each field is [`Option`] so "not set" is distinguishable from "set to
//! the default", which is the signal [`Settings::merge`] uses to decide
//! whether an override wins. Consumers read via
//! `settings.field.unwrap_or(default)` at the point of use.

use crate::ast::{Config, EventType, Setting, Statement, Value};

/// Default placement for a newly-opened Claude chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudePlacement {
    Pane,
    DockLeft,
    DockRight,
}

/// Mouse-capture policy applied at terminal startup. `Auto` keeps the
/// parent-multiplexer guard (capture disabled when `$TMUX` or `$ZELLIJ`
/// is set so the parent owns drag-select); `Always` forces capture on
/// regardless of nesting; `Never` disables it unconditionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseCapturePolicy {
    Auto,
    Always,
    Never,
}

/// Top-level resolved settings struct.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Settings {
    /// Enables the Claude Code / LSP text-protocol transcript log.
    pub text_proto_log: Option<bool>,
    /// Default placement of `OpenClaude`. `None` means "pane".
    pub claude_default_placement: Option<ClaudePlacement>,
    /// Name of the active theme block. Resolves against `theme NAME { ... }`
    /// blocks in the config. `None` means "use the compiled-in default".
    pub theme: Option<String>,
    /// Mouse-capture policy at terminal startup. `None` falls back to
    /// [`MouseCapturePolicy::Auto`].
    pub mouse_capture: Option<MouseCapturePolicy>,
}

impl Settings {
    /// Extracts known settings from `on init` blocks in `config`. Unknown
    /// setting paths are silently ignored so a config file that references
    /// a future setting on an older binary does not fail to parse.
    pub fn from_config(config: &Config) -> Self {
        let mut out = Settings::default();
        for block in &config.blocks {
            if block.node.event != EventType::Init {
                continue;
            }
            for stmt in &block.node.statements {
                if let Statement::Setting(setting) = &stmt.node {
                    out.apply(setting);
                }
            }
        }
        out
    }

    /// Right-hand wins: any `Some(_)` field in `other` overrides the
    /// corresponding field in `self`. Used to layer CLI over stcfg.
    pub fn merge(self, other: Settings) -> Settings {
        Settings {
            text_proto_log: other.text_proto_log.or(self.text_proto_log),
            claude_default_placement: other
                .claude_default_placement
                .or(self.claude_default_placement),
            theme: other.theme.or(self.theme),
            mouse_capture: other.mouse_capture.or(self.mouse_capture),
        }
    }

    fn apply(&mut self, setting: &Setting) {
        let path: Vec<&str> = setting.path.iter().map(|p| p.node.as_str()).collect();
        match path.as_slice() {
            ["text_proto_log"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.text_proto_log = Some(b);
                }
            },
            ["claude", "default_placement"] => {
                let raw = match &setting.value.node {
                    Value::String(s) | Value::Ident(s) => Some(s.as_str()),
                    _ => None,
                };
                let placement = match raw {
                    Some("pane") => Some(ClaudePlacement::Pane),
                    Some("dock-left") => Some(ClaudePlacement::DockLeft),
                    Some("dock-right") => Some(ClaudePlacement::DockRight),
                    _ => None,
                };
                if let Some(p) = placement {
                    self.claude_default_placement = Some(p);
                }
            },
            ["theme"] => {
                if let Value::Ident(s) | Value::String(s) = &setting.value.node {
                    self.theme = Some(s.clone());
                }
            },
            ["mouse", "capture"] => {
                let raw = match &setting.value.node {
                    Value::String(s) | Value::Ident(s) => Some(s.as_str()),
                    _ => None,
                };
                let policy = match raw {
                    Some("auto") => Some(MouseCapturePolicy::Auto),
                    Some("always") => Some(MouseCapturePolicy::Always),
                    Some("never") => Some(MouseCapturePolicy::Never),
                    _ => None,
                };
                if let Some(p) = policy {
                    self.mouse_capture = Some(p);
                }
            },
            _ => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn parse_ok(source: &str) -> Config {
        let (config, errors) = parse(source);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        config.expect("expected successful parse")
    }

    #[test]
    fn from_config_extracts_text_proto_log() {
        let config = parse_ok("on init { text_proto_log = true; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_false_value() {
        let config = parse_ok("on init { text_proto_log = false; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(false),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_ignores_unknown_paths() {
        let config = parse_ok("on init { some.unknown.path = true; text_proto_log = true; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_ignores_non_init_blocks() {
        let config = parse_ok("on key { text_proto_log = true; }");
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn from_config_wrong_value_type_ignored() {
        let config = parse_ok(r#"on init { text_proto_log = "yes"; }"#);
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn merge_right_wins_over_some() {
        let left = Settings {
            text_proto_log: Some(false),
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
        };
        let right = Settings {
            text_proto_log: Some(true),
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn merge_right_none_preserves_left() {
        let left = Settings {
            text_proto_log: Some(true),
            claude_default_placement: None,
            theme: None,
            mouse_capture: None,
        };
        let right = Settings::default();
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                claude_default_placement: None,
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn merge_both_none_is_none() {
        assert_eq!(
            Settings::default().merge(Settings::default()),
            Settings::default()
        );
    }

    #[test]
    fn from_config_extracts_claude_default_placement_pane() {
        let config = parse_ok(r#"on init { claude.default_placement = "pane"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::Pane),
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_extracts_claude_default_placement_dock_left() {
        let config = parse_ok(r#"on init { claude.default_placement = "dock-left"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockLeft),
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_extracts_claude_default_placement_dock_right() {
        let config = parse_ok(r#"on init { claude.default_placement = "dock-right"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockRight),
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_ignores_unknown_placement_value() {
        let config = parse_ok(r#"on init { claude.default_placement = "elsewhere"; }"#);
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn from_config_ignores_wrong_type_placement_value() {
        let config = parse_ok("on init { claude.default_placement = true; }");
        assert_eq!(Settings::from_config(&config), Settings::default());
    }

    #[test]
    fn merge_preserves_claude_placement() {
        let left = Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockRight),
            theme: None,
            mouse_capture: None,
        };
        let right = Settings::default();
        assert_eq!(
            left.clone().merge(right),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockRight),
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_extracts_theme_ident() {
        let config = parse_ok("on init { theme = default_dark; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_extracts_theme_string() {
        let config = parse_ok(r#"on init { theme = "default_dark"; }"#);
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                claude_default_placement: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn merge_right_overrides_theme() {
        let left = Settings {
            text_proto_log: None,
            claude_default_placement: None,
            theme: Some("a".into()),
            mouse_capture: None,
        };
        let right = Settings {
            text_proto_log: None,
            claude_default_placement: None,
            theme: Some("b".into()),
            mouse_capture: None,
        };
        assert_eq!(left.merge(right).theme, Some("b".into()));
    }

    #[test]
    fn merge_right_overrides_claude_placement() {
        let left = Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::Pane),
            theme: None,
            mouse_capture: None,
        };
        let right = Settings {
            text_proto_log: None,
            claude_default_placement: Some(ClaudePlacement::DockLeft),
            theme: None,
            mouse_capture: None,
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: None,
                claude_default_placement: Some(ClaudePlacement::DockLeft),
                theme: None,
                mouse_capture: None,
            }
        );
    }

    #[test]
    fn from_config_extracts_mouse_capture_auto() {
        let config = parse_ok(r#"on init { mouse.capture = "auto"; }"#);
        assert_eq!(
            Settings::from_config(&config).mouse_capture,
            Some(MouseCapturePolicy::Auto),
        );
    }

    #[test]
    fn from_config_extracts_mouse_capture_always() {
        let config = parse_ok(r#"on init { mouse.capture = "always"; }"#);
        assert_eq!(
            Settings::from_config(&config).mouse_capture,
            Some(MouseCapturePolicy::Always),
        );
    }

    #[test]
    fn from_config_extracts_mouse_capture_never_ident() {
        let config = parse_ok("on init { mouse.capture = never; }");
        assert_eq!(
            Settings::from_config(&config).mouse_capture,
            Some(MouseCapturePolicy::Never),
        );
    }

    #[test]
    fn from_config_ignores_unknown_mouse_capture_value() {
        let config = parse_ok(r#"on init { mouse.capture = "elsewhere"; }"#);
        assert_eq!(Settings::from_config(&config).mouse_capture, None);
    }

    #[test]
    fn merge_right_overrides_mouse_capture() {
        let left = Settings {
            mouse_capture: Some(MouseCapturePolicy::Auto),
            ..Settings::default()
        };
        let right = Settings {
            mouse_capture: Some(MouseCapturePolicy::Never),
            ..Settings::default()
        };
        assert_eq!(
            left.merge(right).mouse_capture,
            Some(MouseCapturePolicy::Never)
        );
    }
}
