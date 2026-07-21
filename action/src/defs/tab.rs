use crate::{
    action::define_action, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
    ValueSource,
};
use std::any::Any;

const GOTO_TAB_PARAMS: &[ParamDef] = &[ParamDef {
    name: "index",
    kind: ParamKind::Number,
    value_source: ValueSource::None,
    required: true,
    description: "1-based tab position in display order.",
}];

#[derive(Debug)]
pub struct GotoTabDef;

impl ActionDef for GotoTabDef {
    fn name(&self) -> &'static str {
        "GotoTab"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::GotoTab
    }

    fn params(&self) -> &'static [ParamDef] {
        GOTO_TAB_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "switch to tab by number"
    }

    fn long_desc(&self) -> &'static str {
        "Switch to the tab at the given 1-based position in display order. An \
         index past the last tab reports it in the status line and leaves the \
         current tab alone."
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["tab"]
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Common
    }
}

#[derive(Debug)]
pub struct GotoTab {
    pub index: usize,
}

impl GotoTab {
    pub const DEF: &GotoTabDef = &GotoTabDef;
}

impl Action for GotoTab {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    NewTabDef,
    NewTab,
    "NewTab",
    ActionKind::NewTab,
    "open a new tab",
    "Append a tab holding a single pane on a fresh scratch buffer and switch \
     to it. Buffers, editors, and terminals are shared across a workspace's \
     tabs, so the new tab can show any of them.",
    ActionPriority::Common,
    aliases = &["tab-new"]
);

define_action!(
    CloseTabDef,
    CloseTab,
    "CloseTab",
    ActionKind::CloseTab,
    "close the current tab",
    "Close the active tab and switch to the most recently used one, or the \
     nearest neighbour when there is none. Refuses on the last tab, since a \
     workspace always needs somewhere to show panes.",
    ActionPriority::Common,
    aliases = &["tab-close"]
);

define_action!(
    ToggleTabDef,
    ToggleTab,
    "ToggleTab",
    ActionKind::ToggleTab,
    "toggle the two most recent tabs",
    "Switch back to the tab that was active before this one, so repeating it \
     alternates between the pair. Reports in the status line when nothing has \
     been switched away from yet.",
    ActionPriority::Common,
    aliases = &["tab-toggle"]
);

define_action!(
    ToggleTabBarDef,
    ToggleTabBar,
    "ToggleTabBar",
    ActionKind::ToggleTabBar,
    "show or hide the tab bar",
    "Show the tab bar when it is hidden, or hide it when it is showing, for \
     this session only. The `ui.tab_bar` setting decides what it does by \
     default, and a restart returns to that.",
    ActionPriority::Common,
    aliases = &["tabs"]
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_names() {
        assert_eq!(GotoTab { index: 1 }.kind(), ActionKind::GotoTab);
        assert_eq!(GotoTab::DEF.name(), "GotoTab");
        assert_eq!(GotoTab::DEF.params().len(), 1);
        assert_eq!(GotoTab::DEF.aliases(), ["tab"]);

        assert_eq!(NewTab.kind(), ActionKind::NewTab);
        assert_eq!(CloseTab.kind(), ActionKind::CloseTab);
        assert_eq!(ToggleTab.kind(), ActionKind::ToggleTab);
        for def in [NewTab.def(), CloseTab.def(), ToggleTab.def()] {
            assert!(def.params().is_empty());
            assert!(def.palette_visible());
        }
    }
}
