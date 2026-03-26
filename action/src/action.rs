use crate::{kind::ActionKind, param::ParamDef};
use std::{any::Any, fmt::Debug};

pub trait ActionDef: Debug + Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn kind(&self) -> ActionKind;
    fn params(&self) -> &'static [ParamDef];
    fn short_desc(&self) -> &'static str;
    fn long_desc(&self) -> &'static str;
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
