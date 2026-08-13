use crate::{
    action::define_action, Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind,
    ValueSource,
};
use std::any::Any;

const WALKTHROUGH_OPEN_PARAMS: &[ParamDef] = &[ParamDef {
    name: "slug",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: true,
    description: "Slug of the stored walkthrough to play.",
}];

#[derive(Debug)]
pub struct WalkthroughOpenDef;

impl ActionDef for WalkthroughOpenDef {
    fn name(&self) -> &'static str {
        "WalkthroughOpen"
    }

    fn command_name(&self) -> Option<&'static str> {
        Some("walkthrough")
    }

    fn kind(&self) -> ActionKind {
        ActionKind::WalkthroughOpen
    }

    fn params(&self) -> &'static [ParamDef] {
        WALKTHROUGH_OPEN_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "play a stored walkthrough"
    }

    fn long_desc(&self) -> &'static str {
        "Load the named walkthrough from the workspace and jump to its first \
         stop. Reports why without changing anything when no walkthrough is \
         stored under that slug, or when the one stored has no stops."
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
    }
}

#[derive(Debug)]
pub struct WalkthroughOpen {
    pub slug: String,
}

impl WalkthroughOpen {
    pub const DEF: &WalkthroughOpenDef = &WalkthroughOpenDef;
}

impl Action for WalkthroughOpen {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    WalkthroughNextDef,
    WalkthroughNext,
    "WalkthroughNext",
    ActionKind::WalkthroughNext,
    "go to the next walkthrough stop",
    "Move to the next stop of the walkthrough being played, opening its file \
     and putting the cursor on its focus. Stops at the last one rather than \
     wrapping around to the first.",
    ActionPriority::Normal,
    command_name = "walkthrough-next"
);

define_action!(
    WalkthroughPrevDef,
    WalkthroughPrev,
    "WalkthroughPrev",
    ActionKind::WalkthroughPrev,
    "go to the previous walkthrough stop",
    "Move to the previous stop of the walkthrough being played, opening its \
     file and putting the cursor on its focus. Stops at the first one rather \
     than wrapping around to the last.",
    ActionPriority::Normal,
    command_name = "walkthrough-prev"
);

define_action!(
    WalkthroughDoneDef,
    WalkthroughDone,
    "WalkthroughDone",
    ActionKind::WalkthroughDone,
    "end the walkthrough",
    "End the walkthrough being played, leaving the reader on whichever stop \
     they reached. The tour stays stored and plays again from the start.",
    ActionPriority::Rare,
    command_name = "walkthrough-done"
);
