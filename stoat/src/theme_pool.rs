//! The set of themes a running editor can switch between, and the rule for
//! when each one is paid for.
//!
//! Themes reach the editor from three places: the embedded config, the user's
//! `config.stcfg`, and VSCode color-theme JSON files. The first two arrive
//! already parsed, because reading those configs parses them anyway. A VSCode
//! theme does not. Converting one costs a JSON parse plus a scan of every token
//! rule per stoat scope, milliseconds apiece for the 50-300KB files themes ship
//! as, and a user's theme directory holds every theme they have ever tried.
//!
//! Only one theme is ever active, so the pool holds VSCode themes as their
//! unconverted source and converts one when something resolves it.

use crate::theme::{Theme, ThemeError};
use std::{
    collections::HashSet,
    sync::{Arc, OnceLock},
};
use stoat_config::{Spanned, ThemeBlock};

/// The themes an editor can resolve by name, in the order they layer.
///
/// Two blocks of the same name layer in pool order, so a user config's
/// `theme one-dark { ... }` overrides an imported theme of that name. Sources
/// therefore have to keep their relative positions rather than being grouped by
/// kind, which is why converted and unconverted themes share one list.
#[derive(Default)]
pub(crate) struct ThemePool {
    entries: Vec<PoolEntry>,
}

/// A VSCode color theme held as the JSON it was read from.
///
/// Shared behind an [`Arc`] so a config reload can rebuild the pool around the
/// same sources, keeping whatever they have already converted.
pub(crate) struct VscodeSource {
    name: String,
    source: String,
    converted: OnceLock<Result<Spanned<ThemeBlock>, String>>,
}

enum PoolEntry {
    Parsed(Spanned<ThemeBlock>),
    Vscode(Arc<VscodeSource>),
}

impl ThemePool {
    /// Add a theme block that is already parsed, at the end of the pool.
    pub(crate) fn push_parsed(&mut self, block: Spanned<ThemeBlock>) {
        self.entries.push(PoolEntry::Parsed(block));
    }

    /// Add a VSCode theme, at the end of the pool, without converting it.
    pub(crate) fn push_vscode(&mut self, source: Arc<VscodeSource>) {
        self.entries.push(PoolEntry::Vscode(source));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every theme name in pool order, including duplicates, converting
    /// nothing.
    ///
    /// A VSCode theme is named by its file stem, which is also the name its
    /// converted block carries, so a name here resolves whether or not the
    /// theme has been converted yet. A theme whose JSON is broken is listed
    /// too, and fails when selected.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(PoolEntry::name)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.names().any(|n| n == name)
    }

    /// Build the theme named `name`, converting only the themes its
    /// inheritance chain reaches.
    ///
    /// Fails with [`ThemeError::ThemeNotFound`] when `name` or one of its
    /// parents names no theme, [`ThemeError::InheritanceCycle`] when the
    /// `inherits` chain loops, or [`ThemeError::ImportFailed`] when a theme on
    /// the chain is VSCode JSON that does not parse.
    pub(crate) fn resolve(&self, name: &str) -> Result<Theme, ThemeError> {
        let wanted = self.inheritance_closure(name)?;

        let mut blocks: Vec<&Spanned<ThemeBlock>> = Vec::new();
        for entry in &self.entries {
            if wanted.contains(entry.name()) {
                blocks.push(entry.block()?);
            }
        }

        Theme::from_blocks(name, &blocks)
    }

    /// The names resolving `name` needs, being `name` itself plus whatever its
    /// blocks inherit, transitively.
    ///
    /// Walking the chain is what forces conversion, since a theme's parent is
    /// only readable once its block exists. Names already collected are not
    /// walked again, so an `inherits` loop terminates here and is reported by
    /// [`Theme::from_blocks`].
    fn inheritance_closure(&self, name: &str) -> Result<HashSet<String>, ThemeError> {
        let mut wanted = HashSet::new();
        let mut pending = vec![name.to_string()];

        while let Some(next) = pending.pop() {
            if !wanted.insert(next.clone()) {
                continue;
            }

            for entry in self.entries.iter().filter(|e| e.name() == next) {
                if let Some(parent) = &entry.block()?.node.parent {
                    pending.push(parent.node.clone());
                }
            }
        }

        Ok(wanted)
    }
}

impl PoolEntry {
    fn name(&self) -> &str {
        match self {
            Self::Parsed(block) => &block.node.name.node,
            Self::Vscode(source) => &source.name,
        }
    }

    fn block(&self) -> Result<&Spanned<ThemeBlock>, ThemeError> {
        match self {
            Self::Parsed(block) => Ok(block),
            Self::Vscode(source) => source.block(),
        }
    }
}

impl VscodeSource {
    pub(crate) fn new(name: String, source: String) -> Self {
        Self {
            name,
            source,
            converted: OnceLock::new(),
        }
    }

    /// Convert on first call, then answer from the conversion.
    ///
    /// A failed conversion is remembered as the failure, so a broken theme
    /// costs one parse attempt no matter how often it is selected.
    fn block(&self) -> Result<&Spanned<ThemeBlock>, ThemeError> {
        let converted = self.converted.get_or_init(|| {
            let theme = vscode_theme::parse(&self.source).map_err(|error| error.to_string())?;
            Ok(crate::theme_vscode::theme_block(&self.name, &theme))
        });

        match converted {
            Ok(block) => Ok(block),
            Err(message) => crate::theme::ImportFailedSnafu {
                name: self.name.clone(),
                message: message.clone(),
            }
            .fail(),
        }
    }

    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    pub(crate) fn is_converted(&self) -> bool {
        self.converted.get().is_some()
    }
}
