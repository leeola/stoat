use crate::{
    defs::{
        app::Quit,
        pane::{
            ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
            SplitRight,
        },
    },
    Action, ActionDef,
};
use std::{collections::HashMap, sync::OnceLock};

pub struct RegistryEntry {
    pub def: &'static dyn ActionDef,
    pub create: fn() -> Box<dyn Action>,
}

static REGISTRY: OnceLock<HashMap<&'static str, RegistryEntry>> = OnceLock::new();

fn init() -> HashMap<&'static str, RegistryEntry> {
    let mut map = HashMap::with_capacity(16);
    let mut add = |def: &'static dyn ActionDef, create: fn() -> Box<dyn Action>| {
        map.insert(def.name(), RegistryEntry { def, create });
    };

    add(Quit::DEF, || Box::new(Quit));
    add(SplitRight::DEF, || Box::new(SplitRight));
    add(SplitDown::DEF, || Box::new(SplitDown));
    add(FocusLeft::DEF, || Box::new(FocusLeft));
    add(FocusRight::DEF, || Box::new(FocusRight));
    add(FocusUp::DEF, || Box::new(FocusUp));
    add(FocusDown::DEF, || Box::new(FocusDown));
    add(FocusNext::DEF, || Box::new(FocusNext));
    add(FocusPrev::DEF, || Box::new(FocusPrev));
    add(ClosePane::DEF, || Box::new(ClosePane));

    map
}

pub fn lookup(name: &str) -> Option<&'static RegistryEntry> {
    REGISTRY.get_or_init(init).get(name)
}

pub fn all() -> impl Iterator<Item = &'static RegistryEntry> {
    REGISTRY.get_or_init(init).values()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_NAMES: &[&str] = &[
        "Quit",
        "SplitRight",
        "SplitDown",
        "FocusLeft",
        "FocusRight",
        "FocusUp",
        "FocusDown",
        "FocusNext",
        "FocusPrev",
        "ClosePane",
    ];

    #[test]
    fn lookup_all_actions() {
        for name in ALL_NAMES {
            assert!(lookup(name).is_some(), "missing: {name}");
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("Foo").is_none());
        assert!(lookup("SetMode").is_none());
    }

    #[test]
    fn factory_creates_correct_kind() {
        for name in ALL_NAMES {
            let entry = lookup(name).expect(name);
            let action = (entry.create)();
            assert_eq!(action.kind(), entry.def.kind(), "kind mismatch for {name}");
        }
    }

    #[test]
    fn all_returns_complete_list() {
        assert_eq!(all().count(), 10);
    }

    #[test]
    fn all_have_descriptions() {
        for entry in all() {
            assert!(!entry.def.short_desc().is_empty(), "{}", entry.def.name());
            assert!(!entry.def.long_desc().is_empty(), "{}", entry.def.name());
        }
    }
}
