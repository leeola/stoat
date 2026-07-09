//! Typed view of stcfg settings, with a merge operator so CLI/env flags
//! can override values loaded from config files.
//!
//! Each field is [`Option`] so "not set" is distinguishable from "set to
//! the default", which is the signal [`Settings::merge`] uses to decide
//! whether an override wins. Consumers read via
//! `settings.field.unwrap_or(default)` at the point of use.

use crate::ast::{Config, EventType, Setting, Statement, Value};
use std::collections::BTreeMap;

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
    /// Enables the LSP text-protocol transcript log.
    pub text_proto_log: Option<bool>,
    /// Runs LSP formatting on the focused buffer before each save when the
    /// server advertises the capability. `None` falls back to disabled. Set
    /// `format_on_save = true;` in stcfg. A format that errors or exceeds the
    /// save-time budget saves the buffer unchanged.
    pub format_on_save: Option<bool>,
    /// Whether an open review session follows the project as it changes:
    /// external edits, git-state changes, and newly-changed files refresh it
    /// automatically. `None` falls back to enabled. Set `review.follow = false;`
    /// in stcfg to require a manual `r` instead.
    pub review_follow: Option<bool>,
    /// Whether the diff cache is warmed in the background so opening review is
    /// near-instant. `None` falls back to enabled. Set `review.precompute =
    /// false;` in stcfg to turn the background diffing off.
    pub review_precompute: Option<bool>,
    /// Name of the active theme block. Resolves against `theme NAME { ... }`
    /// blocks in the config. `None` means "use the compiled-in default".
    pub theme: Option<String>,
    /// Mouse-capture policy at terminal startup. `None` falls back to
    /// [`MouseCapturePolicy::Auto`].
    pub mouse_capture: Option<MouseCapturePolicy>,
    /// Rows the view keeps between the primary cursor and the top or bottom
    /// edge when following it. `None` falls back to 3. Set via
    /// `editor.scrolloff = N;` in stcfg.
    pub scrolloff: Option<u32>,
    /// Whether the editor gutter shows absolute line numbers. `None` falls back
    /// to enabled. Set `editor.line_numbers = false;` in stcfg to restore the
    /// diagnostic-only gutter.
    pub editor_line_numbers: Option<bool>,
    /// Program a terminal pane spawns as its subshell. `None` lets the spawn
    /// site fall back to `$SHELL`, then `/bin/sh`. Set via
    /// `terminal.shell = "/bin/zsh";` in stcfg.
    pub terminal_shell: Option<String>,
    /// Arguments passed to the terminal pane's subshell. `None` spawns with no
    /// arguments. Set via `terminal.args = ["-l"];` in stcfg.
    pub terminal_args: Option<Vec<String>>,
    /// Whether workspaces load their direnv environment automatically. `None`
    /// falls back to enabled. Set `direnv.load = false;` in stcfg to disable
    /// automatic env loading. The manual reload action ignores this.
    pub direnv_load: Option<bool>,
    /// Whether changing the working directory reloads the workspace's direnv
    /// environment. `None` falls back to enabled, and has no effect when
    /// `direnv.load` is off. Set `direnv.reload_on_cd = false;` in stcfg.
    pub direnv_reload_on_cd: Option<bool>,
    /// Per-mode status-line badge label overrides, keyed by mode name.
    /// Set via `ui.mode_badge.<name> = "ABC";` in stcfg. Renderer
    /// consults this map before falling back to its hardcoded badge
    /// table; user-defined modes can supply their own entry here so
    /// the status line shows something more meaningful than `---`.
    pub mode_badges: BTreeMap<String, String>,
    /// Per-language language-server command overrides, keyed by language
    /// name. Each value is an argv whose first element is the executable
    /// and the rest are arguments. Set via
    /// `lsp.server.<language> = ["cmd", "arg"];` in stcfg. An entry wins
    /// over the builtin table. An empty argv disables the server for that
    /// language. A language with no entry falls back to the builtin.
    pub lsp_servers: BTreeMap<String, Vec<String>>,
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
        let mut mode_badges = self.mode_badges;
        mode_badges.extend(other.mode_badges);
        let mut lsp_servers = self.lsp_servers;
        lsp_servers.extend(other.lsp_servers);
        Settings {
            text_proto_log: other.text_proto_log.or(self.text_proto_log),
            format_on_save: other.format_on_save.or(self.format_on_save),
            review_follow: other.review_follow.or(self.review_follow),
            review_precompute: other.review_precompute.or(self.review_precompute),
            theme: other.theme.or(self.theme),
            mouse_capture: other.mouse_capture.or(self.mouse_capture),
            scrolloff: other.scrolloff.or(self.scrolloff),
            editor_line_numbers: other.editor_line_numbers.or(self.editor_line_numbers),
            terminal_shell: other.terminal_shell.or(self.terminal_shell),
            terminal_args: other.terminal_args.or(self.terminal_args),
            direnv_load: other.direnv_load.or(self.direnv_load),
            direnv_reload_on_cd: other.direnv_reload_on_cd.or(self.direnv_reload_on_cd),
            mode_badges,
            lsp_servers,
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
            ["format_on_save"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.format_on_save = Some(b);
                }
            },
            ["review", "follow"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.review_follow = Some(b);
                }
            },
            ["review", "precompute"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.review_precompute = Some(b);
                }
            },
            ["theme"] => {
                if let Value::Ident(s) | Value::String(s) = &setting.value.node {
                    self.theme = Some(s.clone());
                }
            },
            ["ui", "mode_badge", name] => {
                if let Value::String(badge) | Value::Ident(badge) = &setting.value.node {
                    self.mode_badges.insert((*name).to_string(), badge.clone());
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
            ["editor", "scrolloff"] => {
                if let Value::Number(n) = setting.value.node {
                    self.scrolloff = Some(n as u32);
                }
            },
            ["editor", "line_numbers"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.editor_line_numbers = Some(b);
                }
            },
            ["terminal", "shell"] => {
                if let Value::Ident(s) | Value::String(s) = &setting.value.node {
                    self.terminal_shell = Some(s.clone());
                }
            },
            ["terminal", "args"] => {
                if let Value::Array(items) = &setting.value.node {
                    self.terminal_args = Some(
                        items
                            .iter()
                            .filter_map(|item| match &item.node {
                                Value::String(s) | Value::Ident(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect(),
                    );
                }
            },
            ["lsp", "server", language] => {
                if let Value::Array(items) = &setting.value.node {
                    let argv: Vec<String> = items
                        .iter()
                        .filter_map(|item| match &item.node {
                            Value::String(s) | Value::Ident(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    self.lsp_servers.insert((*language).to_string(), argv);
                }
            },
            ["direnv", "load"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.direnv_load = Some(b);
                }
            },
            ["direnv", "reload_on_cd"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.direnv_reload_on_cd = Some(b);
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
                format_on_save: None,
                review_follow: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn from_config_extracts_review_follow() {
        let config = parse_ok("on init { review.follow = false; }");
        assert_eq!(Settings::from_config(&config).review_follow, Some(false));
    }

    #[test]
    fn from_config_extracts_review_precompute() {
        let config = parse_ok("on init { review.precompute = false; }");
        assert_eq!(
            Settings::from_config(&config).review_precompute,
            Some(false)
        );
    }

    #[test]
    fn from_config_extracts_editor_line_numbers() {
        let config = parse_ok("on init { editor.line_numbers = false; }");
        assert_eq!(
            Settings::from_config(&config).editor_line_numbers,
            Some(false)
        );
    }

    #[test]
    fn from_config_extracts_lsp_server() {
        let config = parse_ok(r#"on init { lsp.server.rust = ["ra", "--flag"]; }"#);
        assert_eq!(
            Settings::from_config(&config).lsp_servers,
            BTreeMap::from([(
                "rust".to_string(),
                vec!["ra".to_string(), "--flag".to_string()],
            )]),
        );
    }

    #[test]
    fn from_config_false_value() {
        let config = parse_ok("on init { text_proto_log = false; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(false),
                format_on_save: None,
                review_follow: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
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
                format_on_save: None,
                review_follow: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
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
            format_on_save: None,
            review_follow: None,
            review_precompute: None,
            theme: None,
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: Some(true),
            format_on_save: None,
            review_follow: None,
            review_precompute: None,
            theme: None,
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                format_on_save: None,
                review_follow: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn merge_right_none_preserves_left() {
        let left = Settings {
            text_proto_log: Some(true),
            format_on_save: None,
            review_follow: None,
            review_precompute: None,
            theme: None,
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
        };
        let right = Settings::default();
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                format_on_save: None,
                review_follow: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
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
    fn from_config_extracts_theme_ident() {
        let config = parse_ok("on init { theme = default_dark; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: None,
                format_on_save: None,
                review_follow: None,
                review_precompute: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
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
                format_on_save: None,
                review_follow: None,
                review_precompute: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn merge_right_overrides_theme() {
        let left = Settings {
            text_proto_log: None,
            format_on_save: None,
            review_follow: None,
            review_precompute: None,
            theme: Some("a".into()),
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
        };
        let right = Settings {
            text_proto_log: None,
            format_on_save: None,
            review_follow: None,
            review_precompute: None,
            theme: Some("b".into()),
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
        };
        assert_eq!(left.merge(right).theme, Some("b".into()));
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

    #[test]
    fn from_config_extracts_direnv_load() {
        let config = parse_ok("on init { direnv.load = false; }");
        assert_eq!(Settings::from_config(&config).direnv_load, Some(false));
    }

    #[test]
    fn from_config_extracts_direnv_reload_on_cd() {
        let config = parse_ok("on init { direnv.reload_on_cd = false; }");
        assert_eq!(
            Settings::from_config(&config).direnv_reload_on_cd,
            Some(false)
        );
    }

    #[test]
    fn merge_right_overrides_direnv_load() {
        let left = Settings {
            direnv_load: Some(true),
            ..Settings::default()
        };
        let right = Settings {
            direnv_load: Some(false),
            ..Settings::default()
        };
        assert_eq!(left.merge(right).direnv_load, Some(false));
    }

    #[test]
    fn from_config_extracts_mode_badge_string() {
        let config = parse_ok(r#"on init { ui.mode_badge.foo = "FOO"; }"#);
        let badges = Settings::from_config(&config).mode_badges;
        assert_eq!(
            badges,
            BTreeMap::from([("foo".to_string(), "FOO".to_string())])
        );
    }

    #[test]
    fn from_config_extracts_mode_badge_ident() {
        let config = parse_ok("on init { ui.mode_badge.bar = BAR; }");
        let badges = Settings::from_config(&config).mode_badges;
        assert_eq!(
            badges,
            BTreeMap::from([("bar".to_string(), "BAR".to_string())])
        );
    }

    #[test]
    fn from_config_extracts_multiple_mode_badges() {
        let config = parse_ok(
            r#"on init {
                ui.mode_badge.foo = "FOO";
                ui.mode_badge.bar = "BAR";
            }"#,
        );
        let badges = Settings::from_config(&config).mode_badges;
        assert_eq!(
            badges,
            BTreeMap::from([
                ("foo".to_string(), "FOO".to_string()),
                ("bar".to_string(), "BAR".to_string()),
            ])
        );
    }

    #[test]
    fn from_config_ignores_wrong_type_mode_badge() {
        let config = parse_ok("on init { ui.mode_badge.foo = true; }");
        assert!(Settings::from_config(&config).mode_badges.is_empty());
    }

    #[test]
    fn merge_extends_mode_badges_with_right_winning() {
        let left = Settings {
            mode_badges: BTreeMap::from([
                ("foo".to_string(), "FOO".to_string()),
                ("shared".to_string(), "L".to_string()),
            ]),
            ..Settings::default()
        };
        let right = Settings {
            mode_badges: BTreeMap::from([
                ("bar".to_string(), "BAR".to_string()),
                ("shared".to_string(), "R".to_string()),
            ]),
            ..Settings::default()
        };
        assert_eq!(
            left.merge(right).mode_badges,
            BTreeMap::from([
                ("foo".to_string(), "FOO".to_string()),
                ("bar".to_string(), "BAR".to_string()),
                ("shared".to_string(), "R".to_string()),
            ])
        );
    }

    #[test]
    fn from_config_extracts_terminal_shell() {
        let config = parse_ok(r#"on init { terminal.shell = "/bin/zsh"; }"#);
        assert_eq!(
            Settings::from_config(&config).terminal_shell,
            Some("/bin/zsh".to_string())
        );
    }

    #[test]
    fn from_config_extracts_terminal_args() {
        let config = parse_ok(r#"on init { terminal.args = ["-l", "-c"]; }"#);
        assert_eq!(
            Settings::from_config(&config).terminal_args,
            Some(vec!["-l".to_string(), "-c".to_string()])
        );
    }

    #[test]
    fn from_config_terminal_args_skips_non_string_elements() {
        let config = parse_ok(r#"on init { terminal.args = ["-l", 3, "-c"]; }"#);
        assert_eq!(
            Settings::from_config(&config).terminal_args,
            Some(vec!["-l".to_string(), "-c".to_string()])
        );
    }

    #[test]
    fn merge_right_overrides_terminal_shell_and_args() {
        let left = Settings {
            terminal_shell: Some("/bin/bash".to_string()),
            terminal_args: Some(vec!["-i".to_string()]),
            ..Settings::default()
        };
        let right = Settings {
            terminal_shell: Some("/bin/zsh".to_string()),
            terminal_args: Some(vec!["-l".to_string()]),
            ..Settings::default()
        };
        let merged = left.merge(right);
        assert_eq!(merged.terminal_shell, Some("/bin/zsh".to_string()));
        assert_eq!(merged.terminal_args, Some(vec!["-l".to_string()]));
    }
}
