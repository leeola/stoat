/// Update or insert the active-theme setting in an stcfg source,
/// returning the new source.
///
/// When a top-level `theme = <value>` assignment is present its value is
/// replaced in place, preserving the line's indentation and anything
/// from the `;` onward (e.g. a trailing comment). When no such
/// assignment exists an `on init { theme = "<name>"; }` block is
/// appended. All other content -- comments, other keys, surrounding
/// blocks -- is left byte-for-byte intact, so a hand-edited user config
/// keeps its formatting.
pub fn set_theme(existing: &str, name: &str) -> String {
    let value = format!("\"{name}\"");

    let mut replaced = false;
    let mut lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        match (replaced, rewrite_theme_assignment(line, &value)) {
            (false, Some(rewritten)) => {
                lines.push(rewritten);
                replaced = true;
            },
            _ => lines.push(line.to_owned()),
        }
    }

    let mut out = lines.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    if replaced {
        return out;
    }

    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("on init {{\n    theme = {value};\n}}\n"));
    out
}

/// If `line` is a top-level `theme = <value>` assignment, return it with
/// the value replaced by `value`, keeping the leading indentation and
/// the `;`-onward tail. Returns [`None`] for any other line, including
/// dotted paths (`theme.x = ...`), look-alikes (`themed = ...`), and
/// comments.
fn rewrite_theme_assignment(line: &str, value: &str) -> Option<String> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, body) = line.split_at(indent_len);
    let after_eq = body.strip_prefix("theme")?.trim_start().strip_prefix('=')?;
    let tail = after_eq.find(';').map(|i| &after_eq[i..]).unwrap_or("");
    Some(format!("{indent}theme = {value}{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Settings};

    #[test]
    fn creates_block_when_source_is_empty() {
        assert_eq!(
            set_theme("", "solarized"),
            "on init {\n    theme = \"solarized\";\n}\n"
        );
    }

    #[test]
    fn replaces_existing_value_in_place() {
        let src = "on init {\n    theme = default_dark;\n}\n";
        let expected = "on init {\n    theme = \"solarized\";\n}\n";
        assert_eq!(set_theme(src, "solarized"), expected);
    }

    #[test]
    fn preserves_other_keys_and_trailing_comment() {
        let src =
            "// my config\non init {\n    text_proto_log = true;\n    theme = dark; // active\n}\n";
        let expected = "// my config\non init {\n    text_proto_log = true;\n    theme = \"solarized\"; // active\n}\n";
        assert_eq!(set_theme(src, "solarized"), expected);
    }

    #[test]
    fn appends_block_when_no_theme_key_present() {
        let src = "on init {\n    text_proto_log = true;\n}\n";
        let expected =
            "on init {\n    text_proto_log = true;\n}\non init {\n    theme = \"solarized\";\n}\n";
        assert_eq!(set_theme(src, "solarized"), expected);
    }

    #[test]
    fn ignores_look_alike_keys() {
        let src = "on init {\n    themed = no;\n    theme.bg = no;\n    // theme = no\n}\n";
        let written = set_theme(src, "solarized");
        assert!(
            written.contains("theme = \"solarized\";"),
            "should append a real theme block, got:\n{written}"
        );
        assert!(
            written.contains("themed = no;"),
            "look-alike keys must survive"
        );
        assert!(
            written.contains("// theme = no"),
            "commented theme must survive"
        );
    }

    #[test]
    fn output_round_trips_to_resolved_theme() {
        let written = set_theme("", "solarized");
        let (config, errors) = parse(&written);
        assert!(
            errors.is_empty(),
            "written config must parse cleanly: {errors:?}"
        );
        let resolved = Settings::from_config(&config.expect("parsed config"));
        assert_eq!(resolved.theme.as_deref(), Some("solarized"));
    }
}
