use crate::{
    action::{define_action, define_action_def},
    Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource,
};
use std::any::Any;

const WALKTHROUGH_OPEN_PARAMS: &[ParamDef] = &[ParamDef {
    name: "slug",
    kind: ParamKind::String,
    value_source: ValueSource::Walkthroughs,
    required: true,
    description: "Slug of the stored walkthrough to play.",
}];

define_action_def!(
    WalkthroughOpenDef,
    "WalkthroughOpen",
    ActionKind::WalkthroughOpen,
    "play a stored walkthrough",
    "Load the named walkthrough from the workspace and jump to its first \
     stop. Reports why without changing anything when no walkthrough is \
     stored under that slug, or when the one stored has no stops.",
    ActionPriority::Normal,
    command_name = "walkthrough",
    params = WALKTHROUGH_OPEN_PARAMS
);

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
    WalkthroughNextAnnotationDef,
    WalkthroughNextAnnotation,
    "WalkthroughNextAnnotation",
    ActionKind::WalkthroughNextAnnotation,
    "go to the next annotation of this stop",
    "Move to the next labeled annotation of the stop the walkthrough is on, \
     opening the file it names when that differs from the stop's own. The \
     stop's focus heads the walk, so the first step leaves it and the last \
     annotation ends it.",
    ActionPriority::Normal,
    command_name = "walkthrough-next-annotation"
);

define_action!(
    WalkthroughPrevAnnotationDef,
    WalkthroughPrevAnnotation,
    "WalkthroughPrevAnnotation",
    ActionKind::WalkthroughPrevAnnotation,
    "go to the previous annotation of this stop",
    "Move back through the annotations of the stop the walkthrough is on. A \
     step back off the first annotation returns to the stop's own focus, which \
     heads the walk.",
    ActionPriority::Normal,
    command_name = "walkthrough-prev-annotation"
);

define_action!(
    WalkthroughShowNarrationDef,
    WalkthroughShowNarration,
    "WalkthroughShowNarration",
    ActionKind::WalkthroughShowNarration,
    "show the current stop's narration again",
    "Raise the narration popup for the stop the walkthrough is on. The \
     narration shares the hover popup, which the next key press dismisses, so \
     this puts it back without moving off the stop.",
    ActionPriority::Normal,
    command_name = "walkthrough-narration"
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
