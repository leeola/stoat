//! Keymap configuration and custom key binding tests.

use stoat::{config::KeyBinding, Stoat};

#[test]
fn custom_key_binding_with_input() {
    // Test overriding movement commands
    Stoat::test()
        .with_text("hello world")
        .bind_key("normal", "x", KeyBinding::Command("move_right".into()))
        .type_keys("x")
        .assert_cursor(0, 1)
        .type_keys("xx")
        .assert_cursor(0, 3);
}

#[test]
fn custom_mode_with_multiple_bindings() {
    Stoat::test()
        .with_text("foo\nbar\nbaz")
        .bind_keys()
        .in_mode("normal")
        .key_to_mode("d", "delete")
        .in_mode("delete")
        .key("j", "move_down")
        .key("k", "move_up")
        .key_to_mode("Escape", "normal")
        .apply()
        .type_keys("d")
        .assert_in_mode("delete")
        .type_keys("j")
        .assert_cursor(1, 0)
        .type_keys("j")
        .assert_cursor(2, 0)
        .type_keys("k")
        .assert_cursor(1, 0)
        .type_keys("Escape")
        .assert_in_mode("normal");
}

#[test]
fn define_custom_mode_and_use() {
    Stoat::test()
        .with_text("line1\nline2\nline3")
        .define_mode("jump")
        .display_name("JUMP")
        .key("j", "move_down")
        .key("k", "move_up")
        .key_binding(
            "Escape",
            KeyBinding::Mode {
                mode: "normal".into(),
            },
        )
        .apply()
        .bind_key(
            "normal",
            "J",
            KeyBinding::Mode {
                mode: "jump".into(),
            },
        )
        .type_keys("J")
        .assert_in_mode("jump")
        .type_keys("j")
        .assert_cursor(1, 0)
        .type_keys("j")
        .assert_cursor(2, 0)
        .type_keys("Escape")
        .assert_in_mode("normal");
}

#[test]
fn movement_keys_in_custom_binding() {
    Stoat::test()
        .with_text("line one\nline two\nline three")
        .bind_keys()
        .in_mode("normal")
        .key("J", "move_down")
        .key("K", "move_up")
        .key("H", "move_left")
        .key("L", "move_right")
        .apply()
        .type_keys("J")
        .assert_cursor(1, 0)
        .type_keys("LLLL")
        .assert_cursor(1, 4)
        .type_keys("K")
        .assert_cursor(0, 4)
        .type_keys("HH")
        .assert_cursor(0, 2);
}

#[test]
fn chained_mode_transitions() {
    Stoat::test()
        .define_mode("visual")
        .display_name("VISUAL")
        .key_binding(
            "m",
            KeyBinding::Mode {
                mode: "motion".into(),
            },
        )
        .key_binding(
            "Escape",
            KeyBinding::Mode {
                mode: "normal".into(),
            },
        )
        .apply()
        .define_mode("motion")
        .display_name("MOTION")
        .key("l", "move_right")
        .key("h", "move_left")
        .key_binding(
            "Escape",
            KeyBinding::Mode {
                mode: "normal".into(),
            },
        )
        .apply()
        .bind_key(
            "normal",
            "v",
            KeyBinding::Mode {
                mode: "visual".into(),
            },
        )
        .with_text("test")
        .type_keys("v")
        .assert_in_mode("visual")
        .type_keys("m")
        .assert_in_mode("motion")
        .type_keys("ll")
        .assert_cursor(0, 2)
        .type_keys("Escape")
        .assert_in_mode("normal");
}

#[test]
fn override_default_bindings() {
    Stoat::test()
        .with_text("abc")
        .bind_key("normal", "h", KeyBinding::Command("move_right".into()))
        .bind_key("normal", "l", KeyBinding::Command("move_left".into()))
        .type_keys("h")
        .assert_cursor(0, 1)
        .type_keys("h")
        .assert_cursor(0, 2)
        .type_keys("l")
        .assert_cursor(0, 1)
        .type_keys("l")
        .assert_cursor(0, 0);
}
