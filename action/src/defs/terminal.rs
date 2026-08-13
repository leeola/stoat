use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    TerminalDef,
    Terminal,
    "terminal",
    ActionKind::Terminal,
    "open a terminal pane",
    "Open a subshell in the focused pane. The program and arguments come from the terminal.shell and terminal.args settings, falling back to $SHELL and then /bin/sh.",
    ActionPriority::Common,
    aliases = &["term"]
);
