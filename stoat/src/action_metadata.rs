//! Action metadata registry using GPUI's built-in Action trait methods.
//!
//! This module provides a centralized registry for action metadata that uses
//! GPUI's idiomatic [`Action`] trait methods instead of custom HashMaps. It
//! eliminates duplication by using doc comments as the single source of truth
//! for action documentation.
//!
//! # Architecture
//!
//! - **Single Source of Truth**: Doc comments on action structs (e.g., `/// Move cursor up`)
//! - **Auto-Generation**: [`Action::documentation()`] automatically extracts doc comments
//! - **TypeId Lookup**: Registry maps [`TypeId`] to metadata for runtime queries
//! - **Lazy Initialization**: Static registry initializes on first access via [`LazyLock`]
//!
//! # Usage
//!
//! The registry is used by [`crate::modal::help`] and [`crate::command::palette`]
//! to display action information:
//!
//! ```rust,ignore
//! use std::any::TypeId;
//! use crate::action_metadata;
//!
//! let doc = action_metadata::get_documentation(&TypeId::of::<MoveUp>());
//! assert_eq!(doc, Some("Move cursor up one line"));
//! ```
//!
//! # Migration Status
//!
//! **Migration complete** - All 99 actions now use GPUI's idiomatic [`Action::documentation()`]
//! approach instead of the old HashMap-based metadata system.
//!
//! The old `generate_metadata_maps!` macro that generated HashMap entries has been removed.
//! Manual HashMap entries in [`crate::actions`] are retained for backward compatibility with
//! existing command palette and help modal code that performs TypeId lookups.
//!
//! # Relationship to GPUI
//!
//! GPUI's [`Action`] trait provides metadata methods:
//! - [`Action::documentation()`]: Auto-generated from doc comments
//! - [`Action::name()`]: Action name (e.g., "crate::MoveUp")
//! - [`Action::deprecated_aliases()`]: Old action names
//! - [`Action::deprecation_message()`]: Why action was deprecated
//!
//! Our registry wraps these methods to provide TypeId-based lookup for dynamic
//! action dispatch scenarios where we only have a [`TypeId`] at runtime.

use gpui::Action;
use std::{any::TypeId, collections::HashMap, sync::LazyLock};

/// Registry for action metadata using GPUI's [`Action`] trait methods.
///
/// This registry provides TypeId-based lookup for action documentation,
/// delegating to [`Action::documentation()`] for each registered action type.
/// Unlike the old HashMap approach, this uses doc comments as the single
/// source of truth, eliminating ~2000 lines of duplicated boilerplate.
///
/// # Implementation
///
/// The registry is lazily initialized via [`LazyLock`] and populated with
/// all action types during startup. Each action's documentation is extracted
/// via its [`Action::documentation()`] implementation, which is auto-generated
/// from doc comments by the `#[derive(Action)]` macro.
///
/// # Usage by Other Components
///
/// - [`crate::modal::help`]: Displays action help text
/// - [`crate::command::palette`]: Shows action descriptions
/// - Future components: Any code that needs to look up action metadata by [`TypeId`]
pub struct ActionMetadataRegistry {
    /// Maps action [`TypeId`] to documentation string.
    ///
    /// Documentation is extracted from doc comments via [`Action::documentation()`].
    /// For example, `/// Move cursor up` on [`MoveUp`](crate::actions::MoveUp)
    /// becomes the value in this map.
    documentation: HashMap<TypeId, &'static str>,
}

impl ActionMetadataRegistry {
    /// Creates a new empty registry.
    ///
    /// Typically not called directly - use the static [`REGISTRY`] instead.
    fn new() -> Self {
        Self {
            documentation: HashMap::new(),
        }
    }

    /// Registers an action type, extracting its documentation from [`Action::documentation()`].
    ///
    /// If the action has no documentation (doc comment), it is not added to the registry.
    ///
    /// # Type Parameters
    ///
    /// - `A`: The action type to register, must implement [`Action`]
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// registry.register::<MoveUp>();
    /// // Now get_documentation(&TypeId::of::<MoveUp>()) returns Some("Move cursor up one line")
    /// ```
    fn register<A: Action>(&mut self) {
        if let Some(doc) = A::documentation() {
            self.documentation.insert(TypeId::of::<A>(), doc);
        }
    }

    /// Gets the documentation for an action by its [`TypeId`].
    ///
    /// Returns [`None`] if the action is not registered or has no documentation.
    ///
    /// # Arguments
    ///
    /// - `type_id`: The [`TypeId`] of the action to look up
    ///
    /// # Returns
    ///
    /// The documentation string if found, otherwise [`None`].
    fn get_documentation(&self, type_id: &TypeId) -> Option<&'static str> {
        self.documentation.get(type_id).copied()
    }
}

/// Static global registry, lazily initialized on first access.
///
/// Populated with all registered action types during initialization.
/// Currently contains movement actions (12 total), with more groups
/// being migrated incrementally.
///
/// # Migration Status
///
/// **All actions migrated (99 total) - COMPLETE**
///
/// - Movement actions (12): Registered
/// - Selection actions (10): Registered
/// - Editing actions (7): Registered
/// - Mode actions (6): Registered
/// - File finder actions (5): Registered
/// - Buffer finder actions (5): Registered
/// - Command palette actions (6): Registered
/// - Pane management actions (9): Registered
/// - Application actions (3): Registered
/// - View actions (2): Registered
/// - Help actions (5): Registered
/// - Git status actions (11): Registered
/// - Git diff hunk actions (3): Registered
/// - Diff review actions (9): Registered
/// - Git repository actions (6): Registered
static REGISTRY: LazyLock<ActionMetadataRegistry> = LazyLock::new(|| {
    let mut registry = ActionMetadataRegistry::new();

    // Movement actions (12)
    // These are the first group migrated to the idiomatic approach.
    // They demonstrate using Action::documentation() instead of manual HashMaps.
    registry.register::<crate::actions::MoveUp>();
    registry.register::<crate::actions::MoveDown>();
    registry.register::<crate::actions::MoveLeft>();
    registry.register::<crate::actions::MoveRight>();
    registry.register::<crate::actions::MoveWordLeft>();
    registry.register::<crate::actions::MoveWordRight>();
    registry.register::<crate::actions::MoveToLineStart>();
    registry.register::<crate::actions::MoveToLineEnd>();
    registry.register::<crate::actions::MoveToFileStart>();
    registry.register::<crate::actions::MoveToFileEnd>();
    registry.register::<crate::actions::PageUp>();
    registry.register::<crate::actions::PageDown>();

    // Selection actions (10)
    // Second group migrated. These extend the current selection using cursor movement.
    registry.register::<crate::actions::SelectNextSymbol>();
    registry.register::<crate::actions::SelectPrevSymbol>();
    registry.register::<crate::actions::SelectNextToken>();
    registry.register::<crate::actions::SelectPrevToken>();
    registry.register::<crate::actions::SelectLeft>();
    registry.register::<crate::actions::SelectRight>();
    registry.register::<crate::actions::SelectUp>();
    registry.register::<crate::actions::SelectDown>();
    registry.register::<crate::actions::SelectToLineStart>();
    registry.register::<crate::actions::SelectToLineEnd>();

    // Editing actions (7)
    // Third group migrated. These modify buffer content at cursor position.
    registry.register::<crate::actions::DeleteLeft>();
    registry.register::<crate::actions::DeleteRight>();
    registry.register::<crate::actions::DeleteWordLeft>();
    registry.register::<crate::actions::DeleteWordRight>();
    registry.register::<crate::actions::NewLine>();
    registry.register::<crate::actions::DeleteLine>();
    registry.register::<crate::actions::DeleteToEndOfLine>();

    // Mode actions (6)
    // Fourth group migrated. These control the editor's operational mode.
    registry.register::<crate::actions::EnterInsertMode>();
    registry.register::<crate::actions::EnterNormalMode>();
    registry.register::<crate::actions::EnterVisualMode>();
    registry.register::<crate::actions::EnterSpaceMode>();
    registry.register::<crate::actions::EnterPaneMode>();
    registry.register::<crate::actions::EnterGitFilterMode>();

    // File finder actions (5)
    // Fifth group migrated. These provide file navigation via fuzzy finder.
    registry.register::<crate::actions::OpenFileFinder>();
    registry.register::<crate::actions::FileFinderNext>();
    registry.register::<crate::actions::FileFinderPrev>();
    registry.register::<crate::actions::FileFinderSelect>();
    registry.register::<crate::actions::FileFinderDismiss>();

    // Buffer finder actions (5)
    // Sixth group migrated. These provide buffer switching via fuzzy finder.
    registry.register::<crate::actions::OpenBufferFinder>();
    registry.register::<crate::actions::BufferFinderNext>();
    registry.register::<crate::actions::BufferFinderPrev>();
    registry.register::<crate::actions::BufferFinderSelect>();
    registry.register::<crate::actions::BufferFinderDismiss>();

    // Command palette actions (6)
    // Seventh group migrated. These provide command search and execution.
    registry.register::<crate::actions::OpenCommandPalette>();
    registry.register::<crate::actions::CommandPaletteNext>();
    registry.register::<crate::actions::CommandPalettePrev>();
    registry.register::<crate::actions::CommandPaletteExecute>();
    registry.register::<crate::actions::CommandPaletteDismiss>();
    registry.register::<crate::actions::ToggleCommandPaletteHidden>();

    // Pane management actions (9)
    // Eighth group migrated. These control pane splits and focus.
    registry.register::<crate::actions::SplitUp>();
    registry.register::<crate::actions::SplitDown>();
    registry.register::<crate::actions::SplitLeft>();
    registry.register::<crate::actions::SplitRight>();
    registry.register::<crate::actions::Quit>();
    registry.register::<crate::actions::FocusPaneUp>();
    registry.register::<crate::actions::FocusPaneDown>();
    registry.register::<crate::actions::FocusPaneLeft>();
    registry.register::<crate::actions::FocusPaneRight>();

    // Application actions (3)
    // Ninth group migrated. These control app lifecycle and file operations.
    registry.register::<crate::actions::QuitAll>();
    registry.register::<crate::actions::WriteFile>();
    registry.register::<crate::actions::WriteAll>();

    // View actions (2)
    // Tenth group migrated. These control editor view settings.
    registry.register::<crate::actions::ToggleMinimap>();
    registry.register::<crate::actions::ShowMinimapOnScroll>();

    // Help actions (5)
    // Eleventh group migrated. These provide help and about information.
    registry.register::<crate::actions::OpenHelpOverlay>();
    registry.register::<crate::actions::OpenHelpModal>();
    registry.register::<crate::actions::HelpModalDismiss>();
    registry.register::<crate::actions::OpenAboutModal>();
    registry.register::<crate::actions::AboutModalDismiss>();

    // Git status actions (11)
    // Twelfth group migrated. These provide git status modal navigation and filtering.
    registry.register::<crate::actions::OpenGitStatus>();
    registry.register::<crate::actions::GitStatusNext>();
    registry.register::<crate::actions::GitStatusPrev>();
    registry.register::<crate::actions::GitStatusSelect>();
    registry.register::<crate::actions::GitStatusDismiss>();
    registry.register::<crate::actions::GitStatusCycleFilter>();
    registry.register::<crate::actions::GitStatusSetFilterAll>();
    registry.register::<crate::actions::GitStatusSetFilterStaged>();
    registry.register::<crate::actions::GitStatusSetFilterUnstaged>();
    registry.register::<crate::actions::GitStatusSetFilterUnstagedWithUntracked>();
    registry.register::<crate::actions::GitStatusSetFilterUntracked>();

    // Git diff hunk actions (3)
    // Thirteenth group migrated. These provide inline diff viewing and hunk navigation.
    registry.register::<crate::actions::ToggleDiffHunk>();
    registry.register::<crate::actions::GotoNextHunk>();
    registry.register::<crate::actions::GotoPrevHunk>();

    // Diff review actions (9)
    // Fourteenth group migrated. These provide systematic diff review workflow.
    registry.register::<crate::actions::OpenDiffReview>();
    registry.register::<crate::actions::DiffReviewNextHunk>();
    registry.register::<crate::actions::DiffReviewPrevHunk>();
    registry.register::<crate::actions::DiffReviewApproveHunk>();
    registry.register::<crate::actions::DiffReviewToggleApproval>();
    registry.register::<crate::actions::DiffReviewNextUnreviewedHunk>();
    registry.register::<crate::actions::DiffReviewResetProgress>();
    registry.register::<crate::actions::DiffReviewDismiss>();
    registry.register::<crate::actions::DiffReviewCycleComparisonMode>();
    registry.register::<crate::actions::DiffReviewPreviousCommit>();
    registry.register::<crate::actions::DiffReviewRevertHunk>();

    // Git repository actions (6)
    // Fifteenth group migrated. These provide git staging operations.
    registry.register::<crate::actions::GitStageFile>();
    registry.register::<crate::actions::GitStageAll>();
    registry.register::<crate::actions::GitUnstageFile>();
    registry.register::<crate::actions::GitUnstageAll>();
    registry.register::<crate::actions::GitStageHunk>();
    registry.register::<crate::actions::GitUnstageHunk>();
    registry.register::<crate::actions::GitToggleStageHunk>();
    registry.register::<crate::actions::GitToggleStageLine>();

    // Command line actions (1)
    registry.register::<crate::actions::PrintWorkingDirectory>();

    registry
});

/// Gets the documentation for an action by its [`TypeId`].
///
/// This is the primary public API for looking up action documentation.
/// Used by [`crate::modal::help`] and [`crate::command::palette`] to display
/// action information to users.
///
/// # Arguments
///
/// - `type_id`: The [`TypeId`] of the action to look up
///
/// # Returns
///
/// The documentation string if the action is registered and has documentation,
/// otherwise [`None`].
///
/// # Example
///
/// ```rust,ignore
/// use std::any::TypeId;
/// use crate::action_metadata;
/// use crate::actions::MoveUp;
///
/// let doc = action_metadata::get_documentation(&TypeId::of::<MoveUp>());
/// assert_eq!(doc, Some("Move cursor up one line"));
/// ```
///
/// # Integration Points
///
/// - Called by help modal to show action descriptions
/// - Called by command palette to display command help
/// - Future: Could be used by keybinding hints, tutorials, etc.
pub fn get_documentation(type_id: &TypeId) -> Option<&'static str> {
    REGISTRY.get_documentation(type_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{MoveDown, MoveLeft, MoveRight, MoveUp};

    #[test]
    fn registry_provides_documentation_for_movement_actions() {
        let move_up_doc = get_documentation(&TypeId::of::<MoveUp>());
        assert!(
            move_up_doc.is_some(),
            "MoveUp should have documentation from Action::documentation()"
        );

        let move_down_doc = get_documentation(&TypeId::of::<MoveDown>());
        assert!(
            move_down_doc.is_some(),
            "MoveDown should have documentation"
        );

        let move_left_doc = get_documentation(&TypeId::of::<MoveLeft>());
        assert!(
            move_left_doc.is_some(),
            "MoveLeft should have documentation"
        );

        let move_right_doc = get_documentation(&TypeId::of::<MoveRight>());
        assert!(
            move_right_doc.is_some(),
            "MoveRight should have documentation"
        );
    }

    #[test]
    fn documentation_matches_doc_comments() {
        let move_up_doc =
            get_documentation(&TypeId::of::<MoveUp>()).expect("MoveUp should have documentation");

        assert!(
            move_up_doc.contains("Move cursor up"),
            "Documentation should match doc comment. Got: {:?}",
            move_up_doc
        );
    }

    #[test]
    fn registry_provides_documentation_for_selection_actions() {
        let select_left_doc = get_documentation(&TypeId::of::<crate::actions::SelectLeft>());
        assert!(
            select_left_doc.is_some(),
            "SelectLeft should have documentation from Action::documentation()"
        );

        let select_right_doc = get_documentation(&TypeId::of::<crate::actions::SelectRight>());
        assert!(
            select_right_doc.is_some(),
            "SelectRight should have documentation"
        );

        let select_up_doc = get_documentation(&TypeId::of::<crate::actions::SelectUp>());
        assert!(
            select_up_doc.is_some(),
            "SelectUp should have documentation"
        );

        let select_down_doc = get_documentation(&TypeId::of::<crate::actions::SelectDown>());
        assert!(
            select_down_doc.is_some(),
            "SelectDown should have documentation"
        );

        let doc = select_left_doc.unwrap();
        assert!(
            doc.contains("Extend selection"),
            "Documentation should match doc comment. Got: {:?}",
            doc
        );
    }

    #[test]
    fn registry_provides_documentation_for_editing_actions() {
        let delete_left_doc = get_documentation(&TypeId::of::<crate::actions::DeleteLeft>());
        assert!(
            delete_left_doc.is_some(),
            "DeleteLeft should have documentation from Action::documentation()"
        );

        let delete_right_doc = get_documentation(&TypeId::of::<crate::actions::DeleteRight>());
        assert!(
            delete_right_doc.is_some(),
            "DeleteRight should have documentation"
        );

        let new_line_doc = get_documentation(&TypeId::of::<crate::actions::NewLine>());
        assert!(new_line_doc.is_some(), "NewLine should have documentation");

        let delete_line_doc = get_documentation(&TypeId::of::<crate::actions::DeleteLine>());
        assert!(
            delete_line_doc.is_some(),
            "DeleteLine should have documentation"
        );

        let doc = delete_left_doc.unwrap();
        assert!(
            doc.contains("Delete character"),
            "Documentation should match doc comment. Got: {:?}",
            doc
        );
    }

    #[test]
    fn registry_provides_documentation_for_mode_actions() {
        let insert_mode_doc = get_documentation(&TypeId::of::<crate::actions::EnterInsertMode>());
        assert!(
            insert_mode_doc.is_some(),
            "EnterInsertMode should have documentation from Action::documentation()"
        );

        let normal_mode_doc = get_documentation(&TypeId::of::<crate::actions::EnterNormalMode>());
        assert!(
            normal_mode_doc.is_some(),
            "EnterNormalMode should have documentation"
        );

        let visual_mode_doc = get_documentation(&TypeId::of::<crate::actions::EnterVisualMode>());
        assert!(
            visual_mode_doc.is_some(),
            "EnterVisualMode should have documentation"
        );

        let space_mode_doc = get_documentation(&TypeId::of::<crate::actions::EnterSpaceMode>());
        assert!(
            space_mode_doc.is_some(),
            "EnterSpaceMode should have documentation"
        );

        let doc = insert_mode_doc.unwrap();
        assert!(
            doc.contains("Enter insert mode"),
            "Documentation should match doc comment. Got: {:?}",
            doc
        );
    }

    #[test]
    fn registry_provides_documentation_for_file_finder_actions() {
        let open_doc = get_documentation(&TypeId::of::<crate::actions::OpenFileFinder>());
        assert!(
            open_doc.is_some(),
            "OpenFileFinder should have documentation from Action::documentation()"
        );

        let next_doc = get_documentation(&TypeId::of::<crate::actions::FileFinderNext>());
        assert!(
            next_doc.is_some(),
            "FileFinderNext should have documentation"
        );

        let prev_doc = get_documentation(&TypeId::of::<crate::actions::FileFinderPrev>());
        assert!(
            prev_doc.is_some(),
            "FileFinderPrev should have documentation"
        );

        let select_doc = get_documentation(&TypeId::of::<crate::actions::FileFinderSelect>());
        assert!(
            select_doc.is_some(),
            "FileFinderSelect should have documentation"
        );

        let dismiss_doc = get_documentation(&TypeId::of::<crate::actions::FileFinderDismiss>());
        assert!(
            dismiss_doc.is_some(),
            "FileFinderDismiss should have documentation"
        );

        let doc = open_doc.unwrap();
        assert!(
            doc.contains("file finder"),
            "Documentation should match doc comment. Got: {:?}",
            doc
        );
    }

    #[test]
    fn registry_provides_documentation_for_buffer_finder_actions() {
        let open_doc = get_documentation(&TypeId::of::<crate::actions::OpenBufferFinder>());
        assert!(
            open_doc.is_some(),
            "OpenBufferFinder should have documentation from Action::documentation()"
        );

        let next_doc = get_documentation(&TypeId::of::<crate::actions::BufferFinderNext>());
        assert!(
            next_doc.is_some(),
            "BufferFinderNext should have documentation"
        );

        let prev_doc = get_documentation(&TypeId::of::<crate::actions::BufferFinderPrev>());
        assert!(
            prev_doc.is_some(),
            "BufferFinderPrev should have documentation"
        );

        let select_doc = get_documentation(&TypeId::of::<crate::actions::BufferFinderSelect>());
        assert!(
            select_doc.is_some(),
            "BufferFinderSelect should have documentation"
        );

        let dismiss_doc = get_documentation(&TypeId::of::<crate::actions::BufferFinderDismiss>());
        assert!(
            dismiss_doc.is_some(),
            "BufferFinderDismiss should have documentation"
        );

        let doc = open_doc.unwrap();
        assert!(
            doc.contains("buffer finder"),
            "Documentation should match doc comment. Got: {:?}",
            doc
        );
    }

    #[test]
    fn registry_provides_documentation_for_command_palette_actions() {
        let actions = [
            TypeId::of::<crate::actions::OpenCommandPalette>(),
            TypeId::of::<crate::actions::CommandPaletteNext>(),
            TypeId::of::<crate::actions::CommandPalettePrev>(),
            TypeId::of::<crate::actions::CommandPaletteExecute>(),
            TypeId::of::<crate::actions::CommandPaletteDismiss>(),
            TypeId::of::<crate::actions::ToggleCommandPaletteHidden>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Command palette action should have documentation"
            );
        }

        let doc = get_documentation(&actions[0]).unwrap();
        assert!(doc.contains("command palette"));
    }

    #[test]
    fn registry_provides_documentation_for_pane_actions() {
        let actions = [
            TypeId::of::<crate::actions::SplitUp>(),
            TypeId::of::<crate::actions::SplitDown>(),
            TypeId::of::<crate::actions::SplitLeft>(),
            TypeId::of::<crate::actions::SplitRight>(),
            TypeId::of::<crate::actions::Quit>(),
            TypeId::of::<crate::actions::FocusPaneUp>(),
            TypeId::of::<crate::actions::FocusPaneDown>(),
            TypeId::of::<crate::actions::FocusPaneLeft>(),
            TypeId::of::<crate::actions::FocusPaneRight>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Pane action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::SplitRight>()).unwrap();
        assert!(doc.contains("pane") || doc.contains("Split"));
    }

    #[test]
    fn registry_provides_documentation_for_application_actions() {
        let actions = [
            TypeId::of::<crate::actions::QuitAll>(),
            TypeId::of::<crate::actions::WriteFile>(),
            TypeId::of::<crate::actions::WriteAll>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Application action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::WriteFile>()).unwrap();
        assert!(doc.contains("buffer") || doc.contains("Write"));
    }

    #[test]
    fn registry_provides_documentation_for_view_actions() {
        let actions = [
            TypeId::of::<crate::actions::ToggleMinimap>(),
            TypeId::of::<crate::actions::ShowMinimapOnScroll>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "View action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::ToggleMinimap>()).unwrap();
        assert!(doc.contains("minimap"));
    }

    #[test]
    fn registry_provides_documentation_for_help_actions() {
        let actions = [
            TypeId::of::<crate::actions::OpenHelpOverlay>(),
            TypeId::of::<crate::actions::OpenHelpModal>(),
            TypeId::of::<crate::actions::HelpModalDismiss>(),
            TypeId::of::<crate::actions::OpenAboutModal>(),
            TypeId::of::<crate::actions::AboutModalDismiss>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Help action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::OpenHelpOverlay>()).unwrap();
        assert!(doc.contains("help") || doc.contains("Help"));
    }

    #[test]
    fn registry_provides_documentation_for_git_status_actions() {
        let actions = [
            TypeId::of::<crate::actions::OpenGitStatus>(),
            TypeId::of::<crate::actions::GitStatusNext>(),
            TypeId::of::<crate::actions::GitStatusPrev>(),
            TypeId::of::<crate::actions::GitStatusSelect>(),
            TypeId::of::<crate::actions::GitStatusDismiss>(),
            TypeId::of::<crate::actions::GitStatusCycleFilter>(),
            TypeId::of::<crate::actions::GitStatusSetFilterAll>(),
            TypeId::of::<crate::actions::GitStatusSetFilterStaged>(),
            TypeId::of::<crate::actions::GitStatusSetFilterUnstaged>(),
            TypeId::of::<crate::actions::GitStatusSetFilterUnstagedWithUntracked>(),
            TypeId::of::<crate::actions::GitStatusSetFilterUntracked>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Git status action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::OpenGitStatus>()).unwrap();
        assert!(doc.contains("git status") || doc.contains("Git status"));
    }

    #[test]
    fn registry_provides_documentation_for_git_diff_hunk_actions() {
        let actions = [
            TypeId::of::<crate::actions::ToggleDiffHunk>(),
            TypeId::of::<crate::actions::GotoNextHunk>(),
            TypeId::of::<crate::actions::GotoPrevHunk>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Git diff hunk action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::GotoNextHunk>()).unwrap();
        assert!(doc.contains("hunk") || doc.contains("diff"));
    }

    #[test]
    fn registry_provides_documentation_for_diff_review_actions() {
        let actions = [
            TypeId::of::<crate::actions::OpenDiffReview>(),
            TypeId::of::<crate::actions::DiffReviewNextHunk>(),
            TypeId::of::<crate::actions::DiffReviewPrevHunk>(),
            TypeId::of::<crate::actions::DiffReviewApproveHunk>(),
            TypeId::of::<crate::actions::DiffReviewToggleApproval>(),
            TypeId::of::<crate::actions::DiffReviewNextUnreviewedHunk>(),
            TypeId::of::<crate::actions::DiffReviewResetProgress>(),
            TypeId::of::<crate::actions::DiffReviewDismiss>(),
            TypeId::of::<crate::actions::DiffReviewCycleComparisonMode>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Diff review action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::OpenDiffReview>()).unwrap();
        assert!(doc.contains("diff review") || doc.contains("review"));
    }

    #[test]
    fn registry_provides_documentation_for_git_repository_actions() {
        let actions = [
            TypeId::of::<crate::actions::GitStageFile>(),
            TypeId::of::<crate::actions::GitStageAll>(),
            TypeId::of::<crate::actions::GitUnstageFile>(),
            TypeId::of::<crate::actions::GitUnstageAll>(),
            TypeId::of::<crate::actions::GitStageHunk>(),
            TypeId::of::<crate::actions::GitUnstageHunk>(),
            TypeId::of::<crate::actions::GitToggleStageHunk>(),
        ];

        for type_id in &actions {
            assert!(
                get_documentation(type_id).is_some(),
                "Git repository action should have documentation"
            );
        }

        let doc = get_documentation(&TypeId::of::<crate::actions::GitStageFile>()).unwrap();
        assert!(doc.contains("Stage") || doc.contains("stage"));
    }
}
