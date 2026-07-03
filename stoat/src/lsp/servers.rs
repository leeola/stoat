//! Maps a buffer's language to the language server that serves it.
//!
//! The table is the built-in default consulted by the lazy-spawn path in
//! [`crate::action_handlers::lsp::notify_buffer_opened`]. A future settings
//! surface will layer user overrides on top of it.

/// The command and arguments for `language`'s language server, or `None` when
/// no server is known for it.
///
/// `language` is a [`language::Language::name`], e.g. `"rust"`. The returned
/// command is looked up on `PATH` when spawned.
pub(crate) fn server_command(language: &str) -> Option<(&'static str, &'static [&'static str])> {
    match language {
        "rust" => Some(("rust-analyzer", &[])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::server_command;

    #[test]
    fn rust_maps_to_rust_analyzer() {
        let (command, args) = server_command("rust").expect("rust has a server");
        assert_eq!(command, "rust-analyzer");
        assert!(args.is_empty());
    }

    #[test]
    fn unknown_language_has_no_server() {
        assert_eq!(server_command("cobol"), None);
    }
}
