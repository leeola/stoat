//! Tests for the effects system.
//!
//! These tests verify that the correct effects are emitted when various
//! editor state changes occur, particularly around mode transitions and
//! command info display.

use super::*;
use crate::Stoat;

#[test]
fn mode_change_emits_command_context_changed() {
    // Enable command info display first
    let test = Stoat::test().with_text("hello world").type_keys("?"); // Toggle command info on

    // Verify ShowHelp effect was emitted for toggle with correct mode and commands
    let has_show_help = test
        .last_effects()
        .iter()
        .any(|e| matches!(e, Effect::ShowHelp { visible: true, mode, .. } if mode == "Normal"));
    assert!(
        has_show_help,
        "Expected ShowHelp effect when toggling command info"
    );

    // Now switch mode and check for CommandContextChanged
    let test = test.type_keys("i"); // Switch to insert mode

    // When switching modes with command info visible, we should get CommandContextChanged
    let has_context_changed = test
        .last_effects()
        .iter()
        .any(|e| matches!(e, Effect::CommandContextChanged { mode, .. } if mode == "Insert"));
    assert!(
        has_context_changed,
        "Expected CommandContextChanged effect when switching to insert mode, got: {:?}",
        test.last_effects()
    );
}

#[test]
fn no_command_context_changed_when_info_hidden() {
    // Command info is disabled by default
    let test = Stoat::test().with_text("hello world").type_keys("i"); // Switch to insert mode

    // Should NOT have CommandContextChanged effect
    let has_context_changed = test
        .last_effects()
        .iter()
        .any(|e| matches!(e, Effect::CommandContextChanged { .. }));
    assert!(
        !has_context_changed,
        "Should not emit CommandContextChanged when command info is hidden"
    );
}

#[test]
fn custom_mode_bindings_in_help_effect() {
    // First define the custom mode with bindings, including the toggle command info binding
    let test = Stoat::test()
        .with_text("test content")
        .define_mode("custom")
        .key("j", "move_down")
        .key("k", "move_up")
        .key("?", "toggle_command_info") // Add toggle command info to custom mode
        .key_binding(
            "Escape",
            crate::config::KeyBinding::Mode {
                mode: "normal".into(),
            },
        )
        .apply()
        .bind_key(
            "normal",
            "c",
            crate::config::KeyBinding::Mode {
                mode: "custom".into(),
            },
        )
        .type_keys("c"); // Switch to custom mode first

    // Now toggle command info while in custom mode
    let test = test.type_keys("?");

    // Check that ShowHelp contains our custom mode and its bindings
    let found_custom_help = test.last_effects().iter().any(|e| {
        if let Effect::ShowHelp {
            visible: true,
            mode,
            commands,
        } = e
        {
            mode.contains("custom") // The mode is formatted as Custom("custom")
                && commands.iter().any(|(key, _)| key == "j")
                && commands.iter().any(|(key, _)| key == "k")
                && (commands.iter().any(|(key, _)| key == "Escape")
                    || commands.iter().any(|(key, _)| key == "Esc"))
        } else {
            false
        }
    });

    assert!(
        found_custom_help,
        "Custom mode commands should appear in ShowHelp effect, got: {:?}",
        test.last_effects()
    );
}

#[test]
fn mode_transition_shows_updated_help() {
    // Test that ShowHelp effect updates when we transition between modes with command info
    // visible
    let test = Stoat::test().with_text("hello").type_keys("?"); // Enable command info first

    // Verify we're in normal mode with help visible
    assert!(test.last_effects().iter().any(|e| {
        matches!(e, Effect::ShowHelp { visible: true, mode, .. } if mode == "Normal")
    }));

    // Switch to insert mode
    let test = test.type_keys("i");

    // Should get CommandContextChanged for insert mode
    assert!(
        test.last_effects().iter().any(|e| {
            matches!(e, Effect::CommandContextChanged { mode, .. } if mode == "Insert")
        }),
        "Should get CommandContextChanged when switching to insert mode with help visible"
    );

    // Switch back to normal mode
    let test = test.type_keys("<Esc>");

    // Should get CommandContextChanged for normal mode
    assert!(
        test.last_effects().iter().any(|e| {
            matches!(e, Effect::CommandContextChanged { mode, .. } if mode == "Normal")
        }),
        "Should get CommandContextChanged when switching back to normal mode"
    );
}
