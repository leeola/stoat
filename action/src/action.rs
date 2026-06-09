use crate::{kind::ActionKind, param::ParamDef};
use std::{any::Any, fmt::Debug};

/// Default listing priority applied within a match tier in the command
/// palette. Prefix/substring tier ordering dominates; priority is the
/// tie-breaker before alphabetical name order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPriority {
    Common,
    Normal,
    Rare,
}

impl ActionPriority {
    pub fn ord(self) -> u8 {
        match self {
            Self::Common => 0,
            Self::Normal => 1,
            Self::Rare => 2,
        }
    }
}

pub trait ActionDef: Debug + Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn kind(&self) -> ActionKind;
    fn params(&self) -> &'static [ParamDef];
    fn short_desc(&self) -> &'static str;
    fn long_desc(&self) -> &'static str;

    /// Whether this action appears in the command palette listing. Defaults to
    /// true; override to false for plumbing actions that should only be
    /// invoked from keybindings (e.g. opening the palette itself).
    fn palette_visible(&self) -> bool {
        true
    }

    /// Whether this action appears in the floating key-hint banner. Defaults
    /// to true; override to false for launcher and utility actions that
    /// clutter the transient-mode hint. The full help modal lists every
    /// action regardless of this flag.
    fn hint_visible(&self) -> bool {
        true
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
    }

    /// Short alternate names that resolve to this action alongside
    /// [`Self::name`], e.g. `w` for `write`. Empty by default; both the
    /// canonical name and every alias key the same action in the
    /// registry, so a collision with any other name or alias is a
    /// registration-time panic.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
}

pub trait Action: Debug + Send + 'static {
    fn def(&self) -> &'static dyn ActionDef;
    fn kind(&self) -> ActionKind {
        self.def().kind()
    }
    fn as_any(&self) -> &dyn Any;
}

macro_rules! define_action {
    ($def:ident, $action:ident, $name:expr, $kind:expr, $short:expr, $long:expr) => {
        $crate::action::define_action!(
            $def,
            $action,
            $name,
            $kind,
            $short,
            $long,
            $crate::ActionPriority::Normal
        );
    };
    ($def:ident, $action:ident, $name:expr, $kind:expr, $short:expr, $long:expr, $priority:expr) => {
        $crate::action::define_action!($def, $action, $name, $kind, $short, $long, $priority, true);
    };
    ($def:ident, $action:ident, $name:expr, $kind:expr, $short:expr, $long:expr, $priority:expr, $hint_visible:expr) => {
        $crate::action::define_action!(
            $def,
            $action,
            $name,
            $kind,
            $short,
            $long,
            $priority,
            $hint_visible,
            &[]
        );
    };
    ($def:ident, $action:ident, $name:expr, $kind:expr, $short:expr, $long:expr, $priority:expr, $hint_visible:expr, $aliases:expr) => {
        #[derive(Debug)]
        pub struct $def;

        impl $crate::ActionDef for $def {
            fn name(&self) -> &'static str {
                $name
            }

            fn kind(&self) -> $crate::ActionKind {
                $kind
            }

            fn params(&self) -> &'static [$crate::ParamDef] {
                &[]
            }

            fn short_desc(&self) -> &'static str {
                $short
            }

            fn long_desc(&self) -> &'static str {
                $long
            }

            fn priority(&self) -> $crate::ActionPriority {
                $priority
            }

            fn hint_visible(&self) -> bool {
                $hint_visible
            }

            fn aliases(&self) -> &'static [&'static str] {
                $aliases
            }
        }

        #[derive(Debug)]
        pub struct $action;

        impl $action {
            pub const DEF: &$def = &$def;
        }

        impl $crate::Action for $action {
            fn def(&self) -> &'static dyn $crate::ActionDef {
                Self::DEF
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
    };
}

pub(crate) use define_action;
