//! Typed view of stcfg settings, with a merge operator so CLI/env flags
//! can override values loaded from config files.
//!
//! Each field is [`Option`] so "not set" is distinguishable from "set to
//! the default", which is the signal [`Settings::merge`] uses to decide
//! whether an override wins. Consumers read via
//! `settings.field.unwrap_or(default)` at the point of use.

use crate::ast::{Config, EventType, Setting, Spanned, Statement, Value};
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

/// How the editor gutter numbers lines. `Off` hides the number column
/// (diagnostic marks only); `Absolute` shows each line's own number;
/// `Relative` shows the distance from the cursor line (Helix-style).
/// `None` on the setting falls back to `Relative`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineNumbers {
    Off,
    Absolute,
    Relative,
}

/// Which minimap strips editor panes show under stoatty. `Off` hides them.
/// `PerPane` gives each split pane its own right-edge strip. `Single` shows one
/// window-right strip that follows the focused pane. `None` on the setting falls
/// back to `PerPane`, and `false`/`true` are accepted as `Off`/`PerPane`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinimapMode {
    Off,
    PerPane,
    Single,
}

/// How editor panes soft-wrap long lines. `EditorWidth` wraps at the pane's text
/// width. `Bounded` wraps at the smaller of the pane text width and
/// `editor.wrap_column`. `None` disables wrapping, so long lines truncate at the
/// pane edge. `None` on the setting falls back to `EditorWidth`, and
/// `false`/`true` are accepted as `None`/`EditorWidth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    None,
    EditorWidth,
    Bounded,
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
    /// Whether saving a config file re-applies it immediately. `None` falls
    /// back to enabled. Set `config.auto_reload = false;` in stcfg to require a
    /// restart instead.
    ///
    /// It governs both halves of the reload. stoat re-reads its own config
    /// in-process, and the terminal is told to re-read its own when that file
    /// is the one saved.
    pub config_auto_reload: Option<bool>,
    /// Whether an open review session follows the project as it changes:
    /// external edits, git-state changes, and newly-changed files refresh it
    /// automatically. `None` falls back to enabled. Set `review.follow = false;`
    /// in stcfg to require a manual `r` instead.
    pub review_follow: Option<bool>,
    /// Whether a clean-tree diff view, when a rebase is paused (an external
    /// `git rebase` stopped at edit or break), shows the just-applied commit's
    /// diff and follows each rebase step. `None` falls back to enabled. Set
    /// `review.rebase_head = false;` in stcfg to keep the empty clean-tree view.
    pub review_rebase_head: Option<bool>,
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
    /// How the editor gutter numbers lines. `None` falls back to
    /// [`LineNumbers::Relative`]. Set `editor.line_numbers = relative | absolute
    /// | off;` in stcfg (`false` is accepted as `off`, `true` as `relative`).
    pub editor_line_numbers: Option<LineNumbers>,
    /// The minimap strip mode for editor panes under stoatty, one of `off`,
    /// `per_pane`, or `single`. `None` falls back to [`MinimapMode::Single`].
    /// Set `editor.minimap = per_pane;` in stcfg to opt back (`false` means
    /// `off`, `true` means `single`). The `:minimap` command toggles visibility
    /// at runtime.
    pub editor_minimap: Option<MinimapMode>,
    /// How editor panes soft-wrap long lines, one of `none`, `editor_width`, or
    /// `bounded`. `None` falls back at the consumer to [`WrapMode::EditorWidth`].
    /// Set `editor.wrap = none;` in stcfg to disable (`false` means `none`,
    /// `true` means `editor_width`).
    pub editor_wrap: Option<WrapMode>,
    /// The column `bounded` wrap mode wraps at, clamped against the pane text
    /// width. `None` falls back at the consumer to 80. Set `editor.wrap_column =
    /// N;` in stcfg. Consulted only by [`WrapMode::Bounded`].
    pub editor_wrap_column: Option<u32>,
    /// Fraction an unfocused pane's colors blend toward the theme background,
    /// so an inactive split reads as dimmed. `None` falls back to 0.25; `0`
    /// disables dimming. Set `ui.inactive_dim = 0.4;` in stcfg. The raw value
    /// is stored here and clamped to 0.0..=1.0 at the consumer.
    pub ui_inactive_dim: Option<f64>,
    /// How many hidden buffers keep their full highlight state (syntax tree,
    /// tokens) before the least-recently-shown are evicted. `None` falls back to
    /// 64. `0` drops a buffer's state as soon as it is hidden. Set via
    /// `editor.highlight_retention = N;` in stcfg.
    pub highlight_retention: Option<u32>,
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
    /// Whether a direnv diff that only reverts the inherited environment is
    /// applied. `None` falls back to disabled, so launching stoat inside a
    /// shell and opening a directory with no governing `.envrc` keeps the
    /// inherited env instead of unsetting it. Set `direnv.unset_on_exit = true;`
    /// in stcfg to restore the unset. A diff that loads an `.envrc` always
    /// applies regardless.
    pub direnv_unset_on_exit: Option<bool>,
    /// Per-mode status-line badge label overrides, keyed by mode name.
    /// Set via `ui.mode_badge.<name> = "ABC";` in stcfg. Renderer
    /// consults this map before falling back to its hardcoded badge
    /// table; user-defined modes can supply their own entry here so
    /// the status line shows something more meaningful than `---`.
    pub mode_badges: BTreeMap<String, String>,
    /// Per-language language-server command overrides, keyed by language
    /// name. Each value is an argv whose first element is the executable
    /// and the rest are arguments. Set via
    /// `lsp.server.<language> = ["cmd", "arg"];` in stcfg. An entry replaces
    /// the language's primary server, keeping any builtin secondary servers,
    /// and disables the primary when the argv is empty. A language with no
    /// entry falls back to the builtin.
    pub lsp_servers: BTreeMap<String, Vec<String>>,
    /// Ordered per-language server lists, keyed by language name, in routing
    /// priority order. Set via `lsp.servers.<language> = [name, ...];` in stcfg.
    /// A language with an entry uses these named servers instead of the builtin
    /// list. The `lsp.server.<language>` primary override still applies on top.
    pub lsp_server_lists: BTreeMap<String, Vec<String>>,
    /// Named server command definitions, keyed by server name. Set via
    /// `lsp.command.<name> = ["cmd", "arg"];` in stcfg. A name an `lsp.servers`
    /// list references resolves its argv here, else from the builtin table by
    /// name, else the name itself is the command.
    pub lsp_commands: BTreeMap<String, Vec<String>>,
    /// Per-server feature allowlists, keyed by server name. Set via
    /// `lsp.only.<name> = [feature, ...];` in stcfg with Helix kebab-case feature
    /// names. A server with an entry routes only the listed features.
    pub lsp_only: BTreeMap<String, Vec<String>>,
    /// Per-server feature denylists, keyed by server name. Set via
    /// `lsp.except.<name> = [feature, ...];` in stcfg. A server with an entry
    /// routes every feature except those listed.
    pub lsp_except: BTreeMap<String, Vec<String>>,
    /// Named finder scopes, each a list of globs (relative to the workspace
    /// root) that scope lists. Set via `finder.scope.<name> = ["src/**"];` in
    /// stcfg. Shift-Tab in the finder cycles through these after All/Modified.
    /// Empty (the default) means only the builtin scopes exist.
    pub finder_scopes: BTreeMap<String, Vec<String>>,
    /// Name of the finder scope a fresh workspace opens in. `None` falls back
    /// to All. Names a builtin (`all`/`modified`) or a `finder.scope.<name>`
    /// entry. Set via `finder.default_scope = "src";` in stcfg.
    pub finder_default_scope: Option<String>,
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
        let mut lsp_server_lists = self.lsp_server_lists;
        lsp_server_lists.extend(other.lsp_server_lists);
        let mut lsp_commands = self.lsp_commands;
        lsp_commands.extend(other.lsp_commands);
        let mut lsp_only = self.lsp_only;
        lsp_only.extend(other.lsp_only);
        let mut lsp_except = self.lsp_except;
        lsp_except.extend(other.lsp_except);
        let mut finder_scopes = self.finder_scopes;
        finder_scopes.extend(other.finder_scopes);
        Settings {
            text_proto_log: other.text_proto_log.or(self.text_proto_log),
            format_on_save: other.format_on_save.or(self.format_on_save),
            config_auto_reload: other.config_auto_reload.or(self.config_auto_reload),
            review_follow: other.review_follow.or(self.review_follow),
            review_rebase_head: other.review_rebase_head.or(self.review_rebase_head),
            review_precompute: other.review_precompute.or(self.review_precompute),
            theme: other.theme.or(self.theme),
            mouse_capture: other.mouse_capture.or(self.mouse_capture),
            scrolloff: other.scrolloff.or(self.scrolloff),
            editor_line_numbers: other.editor_line_numbers.or(self.editor_line_numbers),
            editor_minimap: other.editor_minimap.or(self.editor_minimap),
            editor_wrap: other.editor_wrap.or(self.editor_wrap),
            editor_wrap_column: other.editor_wrap_column.or(self.editor_wrap_column),
            ui_inactive_dim: other.ui_inactive_dim.or(self.ui_inactive_dim),
            highlight_retention: other.highlight_retention.or(self.highlight_retention),
            terminal_shell: other.terminal_shell.or(self.terminal_shell),
            terminal_args: other.terminal_args.or(self.terminal_args),
            direnv_load: other.direnv_load.or(self.direnv_load),
            direnv_reload_on_cd: other.direnv_reload_on_cd.or(self.direnv_reload_on_cd),
            direnv_unset_on_exit: other.direnv_unset_on_exit.or(self.direnv_unset_on_exit),
            mode_badges,
            lsp_servers,
            lsp_server_lists,
            lsp_commands,
            lsp_only,
            lsp_except,
            finder_scopes,
            finder_default_scope: other.finder_default_scope.or(self.finder_default_scope),
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
            ["config", "auto_reload"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.config_auto_reload = Some(b);
                }
            },
            ["review", "follow"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.review_follow = Some(b);
                }
            },
            ["review", "rebase_head"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.review_rebase_head = Some(b);
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
            ["editor", "highlight_retention"] => {
                if let Value::Number(n) = setting.value.node {
                    self.highlight_retention = Some(n as u32);
                }
            },
            ["editor", "line_numbers"] => {
                let numbers = match &setting.value.node {
                    Value::Bool(false) => Some(LineNumbers::Off),
                    Value::Bool(true) => Some(LineNumbers::Relative),
                    Value::String(s) | Value::Ident(s) => match s.as_str() {
                        "off" => Some(LineNumbers::Off),
                        "absolute" => Some(LineNumbers::Absolute),
                        "relative" => Some(LineNumbers::Relative),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(n) = numbers {
                    self.editor_line_numbers = Some(n);
                }
            },
            ["editor", "minimap"] => {
                let mode = match &setting.value.node {
                    Value::Bool(false) => Some(MinimapMode::Off),
                    Value::Bool(true) => Some(MinimapMode::Single),
                    Value::String(s) | Value::Ident(s) => match s.as_str() {
                        "off" => Some(MinimapMode::Off),
                        "per_pane" => Some(MinimapMode::PerPane),
                        "single" => Some(MinimapMode::Single),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(m) = mode {
                    self.editor_minimap = Some(m);
                }
            },
            ["editor", "wrap"] => {
                let mode = match &setting.value.node {
                    Value::Bool(false) => Some(WrapMode::None),
                    Value::Bool(true) => Some(WrapMode::EditorWidth),
                    Value::String(s) | Value::Ident(s) => match s.as_str() {
                        "none" => Some(WrapMode::None),
                        "editor_width" => Some(WrapMode::EditorWidth),
                        "bounded" => Some(WrapMode::Bounded),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(m) = mode {
                    self.editor_wrap = Some(m);
                }
            },
            ["editor", "wrap_column"] => {
                if let Value::Number(n) = setting.value.node {
                    self.editor_wrap_column = Some(n as u32);
                }
            },
            ["ui", "inactive_dim"] => {
                if let Value::Number(n) = setting.value.node {
                    self.ui_inactive_dim = Some(n);
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
                    self.lsp_servers
                        .insert((*language).to_string(), string_array(items));
                }
            },
            ["lsp", "servers", language] => {
                if let Value::Array(items) = &setting.value.node {
                    self.lsp_server_lists
                        .insert((*language).to_string(), string_array(items));
                }
            },
            ["lsp", "command", name] => {
                if let Value::Array(items) = &setting.value.node {
                    self.lsp_commands
                        .insert((*name).to_string(), string_array(items));
                }
            },
            ["lsp", "only", name] => {
                if let Value::Array(items) = &setting.value.node {
                    self.lsp_only
                        .insert((*name).to_string(), string_array(items));
                }
            },
            ["lsp", "except", name] => {
                if let Value::Array(items) = &setting.value.node {
                    self.lsp_except
                        .insert((*name).to_string(), string_array(items));
                }
            },
            ["finder", "scope", name] => {
                if let Value::Array(items) = &setting.value.node {
                    let globs: Vec<String> = items
                        .iter()
                        .filter_map(|item| match &item.node {
                            Value::String(s) | Value::Ident(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    self.finder_scopes.insert((*name).to_string(), globs);
                }
            },
            ["finder", "default_scope"] => {
                if let Value::Ident(s) | Value::String(s) = &setting.value.node {
                    self.finder_default_scope = Some(s.clone());
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
            ["direnv", "unset_on_exit"] => {
                if let Value::Bool(b) = setting.value.node {
                    self.direnv_unset_on_exit = Some(b);
                }
            },
            _ => {},
        }
    }
}

/// Collect the string and identifier elements of a setting's array value,
/// dropping any non-string elements.
fn string_array(items: &[Spanned<Value>]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match &item.node {
            Value::String(s) | Value::Ident(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
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
                config_auto_reload: None,
                review_follow: None,
                review_rebase_head: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                editor_minimap: None,
                editor_wrap: None,
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                direnv_unset_on_exit: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: None,
            }
        );
    }

    #[test]
    fn from_config_extracts_review_follow() {
        let config = parse_ok("on init { review.follow = false; }");
        assert_eq!(Settings::from_config(&config).review_follow, Some(false));
    }

    #[test]
    fn from_config_extracts_review_rebase_head() {
        let config = parse_ok("on init { review.rebase_head = false; }");
        assert_eq!(
            Settings::from_config(&config).review_rebase_head,
            Some(false)
        );
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
    fn from_config_extracts_config_auto_reload() {
        let config = parse_ok("on init { config.auto_reload = false; }");
        assert_eq!(
            Settings::from_config(&config).config_auto_reload,
            Some(false)
        );
    }

    #[test]
    fn from_config_extracts_editor_line_numbers() {
        let ln = |src: &str| Settings::from_config(&parse_ok(src)).editor_line_numbers;
        assert_eq!(
            ln("on init { editor.line_numbers = false; }"),
            Some(LineNumbers::Off)
        );
        assert_eq!(
            ln("on init { editor.line_numbers = off; }"),
            Some(LineNumbers::Off)
        );
        assert_eq!(
            ln("on init { editor.line_numbers = true; }"),
            Some(LineNumbers::Relative)
        );
        assert_eq!(
            ln("on init { editor.line_numbers = absolute; }"),
            Some(LineNumbers::Absolute)
        );
        assert_eq!(
            ln("on init { editor.line_numbers = relative; }"),
            Some(LineNumbers::Relative)
        );
    }

    #[test]
    fn from_config_extracts_editor_minimap() {
        let mode = |src: &str| Settings::from_config(&parse_ok(src)).editor_minimap;
        assert_eq!(
            mode("on init { editor.minimap = off; }"),
            Some(MinimapMode::Off)
        );
        assert_eq!(
            mode("on init { editor.minimap = per_pane; }"),
            Some(MinimapMode::PerPane)
        );
        assert_eq!(
            mode("on init { editor.minimap = single; }"),
            Some(MinimapMode::Single)
        );
        assert_eq!(
            mode("on init { editor.minimap = false; }"),
            Some(MinimapMode::Off),
            "false is an alias for off"
        );
        assert_eq!(
            mode("on init { editor.minimap = true; }"),
            Some(MinimapMode::Single),
            "true is an alias for the default single"
        );
        assert_eq!(
            mode("on init { }"),
            None,
            "absent falls back at the consumer"
        );
    }

    #[test]
    fn from_config_extracts_editor_wrap() {
        let mode = |src: &str| Settings::from_config(&parse_ok(src)).editor_wrap;
        assert_eq!(
            mode("on init { editor.wrap = none; }"),
            Some(WrapMode::None)
        );
        assert_eq!(
            mode("on init { editor.wrap = editor_width; }"),
            Some(WrapMode::EditorWidth)
        );
        assert_eq!(
            mode("on init { editor.wrap = bounded; }"),
            Some(WrapMode::Bounded)
        );
        assert_eq!(
            mode("on init { editor.wrap = false; }"),
            Some(WrapMode::None),
            "false is an alias for none"
        );
        assert_eq!(
            mode("on init { editor.wrap = true; }"),
            Some(WrapMode::EditorWidth),
            "true is an alias for editor_width"
        );
        assert_eq!(
            mode("on init { }"),
            None,
            "absent falls back at the consumer"
        );
    }

    #[test]
    fn from_config_extracts_editor_wrap_column() {
        let config = parse_ok("on init { editor.wrap_column = 60; }");
        assert_eq!(Settings::from_config(&config).editor_wrap_column, Some(60));
    }

    #[test]
    fn from_config_extracts_ui_inactive_dim() {
        let dim = |src: &str| Settings::from_config(&parse_ok(src)).ui_inactive_dim;
        assert_eq!(dim("on init { ui.inactive_dim = 0.4; }"), Some(0.4));
        assert_eq!(dim("on init { ui.inactive_dim = 0; }"), Some(0.0));
        assert_eq!(dim("on init { }"), None, "absent falls back at consumer");
    }

    #[test]
    fn from_config_extracts_highlight_retention() {
        let config = parse_ok("on init { editor.highlight_retention = 8; }");
        assert_eq!(Settings::from_config(&config).highlight_retention, Some(8));
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
    fn from_config_extracts_lsp_multi_server() {
        let config = parse_ok(
            r#"on init {
                lsp.servers.rust = ["ra", "linter"];
                lsp.command.linter = ["some-linter", "--stdio"];
                lsp.only.linter = ["diagnostics"];
                lsp.except.ra = ["format"];
            }"#,
        );
        let settings = Settings::from_config(&config);
        assert_eq!(
            settings.lsp_server_lists,
            BTreeMap::from([(
                "rust".to_string(),
                vec!["ra".to_string(), "linter".to_string()]
            )]),
        );
        assert_eq!(
            settings.lsp_commands,
            BTreeMap::from([(
                "linter".to_string(),
                vec!["some-linter".to_string(), "--stdio".to_string()]
            )]),
        );
        assert_eq!(
            settings.lsp_only,
            BTreeMap::from([("linter".to_string(), vec!["diagnostics".to_string()])]),
        );
        assert_eq!(
            settings.lsp_except,
            BTreeMap::from([("ra".to_string(), vec!["format".to_string()])]),
        );
    }

    #[test]
    fn from_config_extracts_finder_scope() {
        let config = parse_ok(r#"on init { finder.scope.src = ["src/**", "language/**"]; }"#);
        assert_eq!(
            Settings::from_config(&config).finder_scopes,
            BTreeMap::from([(
                "src".to_string(),
                vec!["src/**".to_string(), "language/**".to_string()],
            )]),
        );
    }

    #[test]
    fn from_config_extracts_finder_default_scope() {
        let config = parse_ok(r#"on init { finder.default_scope = "src"; }"#);
        assert_eq!(
            Settings::from_config(&config).finder_default_scope,
            Some("src".to_string()),
        );
    }

    #[test]
    fn merge_finder_scopes_extend_and_default_right_wins() {
        let left = Settings {
            finder_scopes: BTreeMap::from([("a".to_string(), vec!["a/**".to_string()])]),
            finder_default_scope: Some("a".to_string()),
            ..Settings::default()
        };
        let right = Settings {
            finder_scopes: BTreeMap::from([("b".to_string(), vec!["b/**".to_string()])]),
            finder_default_scope: Some("b".to_string()),
            ..Settings::default()
        };
        let merged = left.merge(right);
        assert_eq!(
            merged.finder_scopes,
            BTreeMap::from([
                ("a".to_string(), vec!["a/**".to_string()]),
                ("b".to_string(), vec!["b/**".to_string()]),
            ]),
        );
        assert_eq!(merged.finder_default_scope, Some("b".to_string()));
    }

    #[test]
    fn from_config_false_value() {
        let config = parse_ok("on init { text_proto_log = false; }");
        assert_eq!(
            Settings::from_config(&config),
            Settings {
                text_proto_log: Some(false),
                format_on_save: None,
                config_auto_reload: None,
                review_follow: None,
                review_rebase_head: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                editor_minimap: None,
                editor_wrap: None,
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                direnv_unset_on_exit: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: None,
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
                config_auto_reload: None,
                review_follow: None,
                review_rebase_head: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                editor_minimap: None,
                editor_wrap: None,
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                direnv_unset_on_exit: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: None,
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
            config_auto_reload: None,
            review_follow: None,
            review_rebase_head: None,
            review_precompute: None,
            theme: None,
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            editor_minimap: None,
            editor_wrap: None,
            editor_wrap_column: None,
            ui_inactive_dim: None,
            highlight_retention: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            direnv_unset_on_exit: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
            lsp_server_lists: BTreeMap::new(),
            lsp_commands: BTreeMap::new(),
            lsp_only: BTreeMap::new(),
            lsp_except: BTreeMap::new(),
            finder_scopes: BTreeMap::new(),
            finder_default_scope: None,
        };
        let right = Settings {
            text_proto_log: Some(true),
            format_on_save: None,
            config_auto_reload: None,
            review_follow: None,
            review_rebase_head: None,
            review_precompute: None,
            theme: None,
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            editor_minimap: None,
            editor_wrap: None,
            editor_wrap_column: None,
            ui_inactive_dim: None,
            highlight_retention: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            direnv_unset_on_exit: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
            lsp_server_lists: BTreeMap::new(),
            lsp_commands: BTreeMap::new(),
            lsp_only: BTreeMap::new(),
            lsp_except: BTreeMap::new(),
            finder_scopes: BTreeMap::new(),
            finder_default_scope: None,
        };
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                format_on_save: None,
                config_auto_reload: None,
                review_follow: None,
                review_rebase_head: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                editor_minimap: None,
                editor_wrap: None,
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                direnv_unset_on_exit: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: None,
            }
        );
    }

    #[test]
    fn merge_right_none_preserves_left() {
        let left = Settings {
            text_proto_log: Some(true),
            format_on_save: None,
            config_auto_reload: None,
            review_follow: None,
            review_rebase_head: None,
            review_precompute: None,
            theme: None,
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            editor_minimap: None,
            editor_wrap: None,
            editor_wrap_column: None,
            ui_inactive_dim: None,
            highlight_retention: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            direnv_unset_on_exit: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
            lsp_server_lists: BTreeMap::new(),
            lsp_commands: BTreeMap::new(),
            lsp_only: BTreeMap::new(),
            lsp_except: BTreeMap::new(),
            finder_scopes: BTreeMap::new(),
            finder_default_scope: None,
        };
        let right = Settings::default();
        assert_eq!(
            left.merge(right),
            Settings {
                text_proto_log: Some(true),
                format_on_save: None,
                config_auto_reload: None,
                review_follow: None,
                review_rebase_head: None,
                review_precompute: None,
                theme: None,
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                editor_minimap: None,
                editor_wrap: None,
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                direnv_unset_on_exit: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: None,
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
                config_auto_reload: None,
                review_follow: None,
                review_rebase_head: None,
                review_precompute: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                editor_minimap: None,
                editor_wrap: None,
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                direnv_unset_on_exit: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: None,
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
                config_auto_reload: None,
                review_follow: None,
                review_rebase_head: None,
                review_precompute: None,
                theme: Some("default_dark".into()),
                mouse_capture: None,
                scrolloff: None,
                editor_line_numbers: None,
                editor_minimap: None,
                editor_wrap: None,
                editor_wrap_column: None,
                ui_inactive_dim: None,
                highlight_retention: None,
                terminal_shell: None,
                terminal_args: None,
                direnv_load: None,
                direnv_reload_on_cd: None,
                direnv_unset_on_exit: None,
                mode_badges: BTreeMap::new(),
                lsp_servers: BTreeMap::new(),
                lsp_server_lists: BTreeMap::new(),
                lsp_commands: BTreeMap::new(),
                lsp_only: BTreeMap::new(),
                lsp_except: BTreeMap::new(),
                finder_scopes: BTreeMap::new(),
                finder_default_scope: None,
            }
        );
    }

    #[test]
    fn merge_right_overrides_theme() {
        let left = Settings {
            text_proto_log: None,
            format_on_save: None,
            config_auto_reload: None,
            review_follow: None,
            review_rebase_head: None,
            review_precompute: None,
            theme: Some("a".into()),
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            editor_minimap: None,
            editor_wrap: None,
            editor_wrap_column: None,
            ui_inactive_dim: None,
            highlight_retention: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            direnv_unset_on_exit: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
            lsp_server_lists: BTreeMap::new(),
            lsp_commands: BTreeMap::new(),
            lsp_only: BTreeMap::new(),
            lsp_except: BTreeMap::new(),
            finder_scopes: BTreeMap::new(),
            finder_default_scope: None,
        };
        let right = Settings {
            text_proto_log: None,
            format_on_save: None,
            config_auto_reload: None,
            review_follow: None,
            review_rebase_head: None,
            review_precompute: None,
            theme: Some("b".into()),
            mouse_capture: None,
            scrolloff: None,
            editor_line_numbers: None,
            editor_minimap: None,
            editor_wrap: None,
            editor_wrap_column: None,
            ui_inactive_dim: None,
            highlight_retention: None,
            terminal_shell: None,
            terminal_args: None,
            direnv_load: None,
            direnv_reload_on_cd: None,
            direnv_unset_on_exit: None,
            mode_badges: BTreeMap::new(),
            lsp_servers: BTreeMap::new(),
            lsp_server_lists: BTreeMap::new(),
            lsp_commands: BTreeMap::new(),
            lsp_only: BTreeMap::new(),
            lsp_except: BTreeMap::new(),
            finder_scopes: BTreeMap::new(),
            finder_default_scope: None,
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
    fn from_config_extracts_direnv_unset_on_exit() {
        let config = parse_ok("on init { direnv.unset_on_exit = true; }");
        assert_eq!(
            Settings::from_config(&config).direnv_unset_on_exit,
            Some(true)
        );
    }

    #[test]
    fn merge_right_overrides_direnv_unset_on_exit() {
        let left = Settings {
            direnv_unset_on_exit: Some(false),
            ..Settings::default()
        };
        let right = Settings {
            direnv_unset_on_exit: Some(true),
            ..Settings::default()
        };
        assert_eq!(left.merge(right).direnv_unset_on_exit, Some(true));
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
