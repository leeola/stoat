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

        impl gpui::Action for $action {
            fn boxed_clone(&self) -> Box<dyn gpui::Action> {
                Box::new(Self)
            }

            fn partial_eq(&self, action: &dyn gpui::Action) -> bool {
                action.as_any().downcast_ref::<Self>().is_some()
            }

            fn name(&self) -> &'static str {
                $name
            }

            fn name_for_type() -> &'static str {
                $name
            }

            fn build(_value: $crate::serde_json::Value) -> gpui::Result<Box<dyn gpui::Action>> {
                Ok(Box::new(Self))
            }
        }

        gpui::register_action!($action);
    };
}

pub(crate) use define_action;

/// Generate `impl gpui::Action` for a parameterized action struct.
/// Requires the struct to derive `Clone`, `PartialEq`, and
/// `serde::Deserialize` so the trait methods can be expressed
/// generically. `$name` is the static action name (matches the
/// struct's `ActionDef::name`).
macro_rules! impl_gpui_action {
    ($t:ident, $name:expr) => {
        impl gpui::Action for $t {
            fn boxed_clone(&self) -> Box<dyn gpui::Action> {
                Box::new(self.clone())
            }

            fn partial_eq(&self, action: &dyn gpui::Action) -> bool {
                action
                    .as_any()
                    .downcast_ref::<Self>()
                    .is_some_and(|other| self == other)
            }

            fn name(&self) -> &'static str {
                $name
            }

            fn name_for_type() -> &'static str {
                $name
            }

            fn build(value: $crate::serde_json::Value) -> gpui::Result<Box<dyn gpui::Action>> {
                let inner: Self = $crate::serde_json::from_value(value)?;
                Ok(Box::new(inner))
            }
        }

        gpui::register_action!($t);
    };
}

pub(crate) use impl_gpui_action;
