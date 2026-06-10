use regex::{Error, Regex, RegexBuilder};

/// Compile `pattern` into a [`Regex`] with multiline mode on, so `^`
/// and `$` match line boundaries inside the buffer text.
pub fn compile_search_regex(pattern: &str) -> Result<Regex, Error> {
    RegexBuilder::new(pattern).multi_line(true).build()
}
