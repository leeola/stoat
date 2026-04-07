use crate::{
    defs::{
        app::Quit,
        file::OpenFile,
        palette::OpenCommandPalette,
        pane::{
            ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
            SplitRight,
        },
    },
    Action, ActionDef, ParamError, ParamKind, ParamValue,
};
use std::{collections::HashMap, path::PathBuf, sync::OnceLock};

pub type CreateFn = fn(&[ParamValue]) -> Result<Box<dyn Action>, ParamError>;

pub struct RegistryEntry {
    pub def: &'static dyn ActionDef,
    pub create: CreateFn,
}

static REGISTRY: OnceLock<HashMap<&'static str, RegistryEntry>> = OnceLock::new();

fn init() -> HashMap<&'static str, RegistryEntry> {
    let mut map = HashMap::with_capacity(16);
    let mut add = |def: &'static dyn ActionDef, create: CreateFn| {
        map.insert(def.name(), RegistryEntry { def, create });
    };

    add(Quit::DEF, |_| Ok(Box::new(Quit)));
    add(SplitRight::DEF, |_| Ok(Box::new(SplitRight)));
    add(SplitDown::DEF, |_| Ok(Box::new(SplitDown)));
    add(FocusLeft::DEF, |_| Ok(Box::new(FocusLeft)));
    add(FocusRight::DEF, |_| Ok(Box::new(FocusRight)));
    add(FocusUp::DEF, |_| Ok(Box::new(FocusUp)));
    add(FocusDown::DEF, |_| Ok(Box::new(FocusDown)));
    add(FocusNext::DEF, |_| Ok(Box::new(FocusNext)));
    add(FocusPrev::DEF, |_| Ok(Box::new(FocusPrev)));
    add(ClosePane::DEF, |_| Ok(Box::new(ClosePane)));
    add(OpenCommandPalette::DEF, |_| {
        Ok(Box::new(OpenCommandPalette))
    });
    add(OpenFile::DEF, |params| {
        let raw = params
            .first()
            .ok_or(ParamError::Missing("path"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "path",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenFile {
            path: PathBuf::from(raw),
        }))
    });

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

    const ZERO_ARG_NAMES: &[&str] = &[
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
        "OpenCommandPalette",
    ];

    #[test]
    fn lookup_all_actions() {
        for name in ZERO_ARG_NAMES {
            assert!(lookup(name).is_some(), "missing: {name}");
        }
        assert!(lookup("OpenFile").is_some());
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("Foo").is_none());
        assert!(lookup("SetMode").is_none());
    }

    #[test]
    fn factory_creates_correct_kind() {
        for name in ZERO_ARG_NAMES {
            let entry = lookup(name).expect(name);
            let action = (entry.create)(&[]).expect(name);
            assert_eq!(action.kind(), entry.def.kind(), "kind mismatch for {name}");
        }
    }

    #[test]
    fn open_file_factory_consumes_path() {
        let entry = lookup("OpenFile").expect("OpenFile");
        let params = vec![ParamValue::String("/tmp/x.rs".into())];
        let action = (entry.create)(&params).expect("create");
        let open = action
            .as_any()
            .downcast_ref::<OpenFile>()
            .expect("downcast");
        assert_eq!(open.path, PathBuf::from("/tmp/x.rs"));
    }

    #[test]
    fn open_file_factory_missing_param_errors() {
        let entry = lookup("OpenFile").expect("OpenFile");
        assert_eq!((entry.create)(&[]).err(), Some(ParamError::Missing("path")));
    }

    #[test]
    fn open_file_factory_wrong_kind_errors() {
        let entry = lookup("OpenFile").expect("OpenFile");
        let err = (entry.create)(&[ParamValue::Number(1.0)]).err();
        assert_eq!(
            err,
            Some(ParamError::WrongKind {
                name: "path",
                expected: ParamKind::String,
            })
        );
    }

    #[test]
    fn all_returns_complete_list() {
        assert_eq!(all().count(), 12);
    }

    #[test]
    fn all_have_descriptions() {
        for entry in all() {
            assert!(!entry.def.short_desc().is_empty(), "{}", entry.def.name());
            assert!(!entry.def.long_desc().is_empty(), "{}", entry.def.name());
        }
    }
}
