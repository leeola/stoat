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

    /// The name this action reads as in the command palette, overriding the
    /// kebab-cased `name` the registry derives by default.
    ///
    /// `None` takes the derived form, which is right for most actions.
    /// Override where mechanical kebab-casing reads badly, or where a shorter
    /// noun is the command's only sensible spelling.
    ///
    /// Keymap and config files address actions by `name`, never by this, since
    /// the config DSL rejects hyphenated identifiers.
    fn command_name(&self) -> Option<&'static str> {
        None
    }

    /// Short alternative tokens that resolve to this action in the command
    /// line, beyond its full `name`.
    ///
    /// Defaults to none. Aliases match case-insensitively. A full action name
    /// always wins over an alias.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    fn priority(&self) -> ActionPriority {
        ActionPriority::Normal
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
    ($def:ident, $action:ident, $name:expr_2021, $kind:expr_2021, $short:expr_2021, $long:expr_2021) => {
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
    ($def:ident, $action:ident, $name:expr_2021, $kind:expr_2021, $short:expr_2021, $long:expr_2021, $priority:expr_2021) => {
        $crate::action::define_action!(
            $def,
            $action,
            $name,
            $kind,
            $short,
            $long,
            $priority,
            palette_visible = true
        );
    };
    ($def:ident, $action:ident, $name:expr_2021, $kind:expr_2021, $short:expr_2021, $long:expr_2021, $priority:expr_2021, palette_visible = $visible:expr_2021) => {
        $crate::action::define_action!(
            $def,
            $action,
            $name,
            $kind,
            $short,
            $long,
            $priority,
            palette_visible = $visible,
            aliases = &[]
        );
    };
    ($def:ident, $action:ident, $name:expr_2021, $kind:expr_2021, $short:expr_2021, $long:expr_2021, $priority:expr_2021, aliases = $aliases:expr_2021) => {
        $crate::action::define_action!(
            $def,
            $action,
            $name,
            $kind,
            $short,
            $long,
            $priority,
            palette_visible = true,
            aliases = $aliases
        );
    };
    ($def:ident, $action:ident, $name:expr_2021, $kind:expr_2021, $short:expr_2021, $long:expr_2021, $priority:expr_2021, palette_visible = $visible:expr_2021, aliases = $aliases:expr_2021) => {
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

            fn palette_visible(&self) -> bool {
                $visible
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
