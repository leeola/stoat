use serde_json::Value;

/// The most rows that a list command gives. A command that prints a huge
/// list gives its first rows, so the palette stays responsive.
const MAX_ROWS: usize = 5_000;

/// A row that the list command of a `pick` argument printed.
///
/// A row of a JSON array has a field for each key of its object and the field
/// `line`. A row of plain text has the field `line` alone. The templates of
/// the argument read the fields by name.
#[derive(Debug, PartialEq)]
pub(crate) struct PickRow {
    fields: Vec<(String, String)>,
}

impl PickRow {
    /// The text of the field `name`, or `None` for a field that the row does
    /// not have.
    pub(crate) fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, text)| text.as_str())
    }
}

/// The rows of `stdout`, the output of a list command.
///
/// A JSON array gives a row for each element. An object gives a field for each
/// of its keys. A string field is the string, null is the empty string, and
/// any other value is its JSON text. Each element also gives the field `line`,
/// its JSON text, or the string itself for a string element. An object's own
/// `line` key comes first.
///
/// Any other text gives a row for each line that holds more than whitespace,
/// with the whole line as the field `line`. Bytes that are not UTF-8 read as
/// replacement chars. At most [`MAX_ROWS`] rows.
pub(crate) fn parse_rows(stdout: &[u8]) -> Vec<PickRow> {
    let text = String::from_utf8_lossy(stdout);
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(elements)) => elements.iter().take(MAX_ROWS).map(element_row).collect(),
        _ => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .take(MAX_ROWS)
            .map(|line| PickRow {
                fields: vec![("line".to_owned(), line.to_owned())],
            })
            .collect(),
    }
}

/// The words of the list command `command` that name it, as the palette's
/// status line names the source of a list.
///
/// The words stop before the first word that starts with `-`, and at three
/// words. So `gh pr list --json number` reads as `gh pr list`.
pub(crate) fn list_head(command: &str) -> String {
    let words: Vec<&str> = command
        .split_whitespace()
        .take_while(|word| !word.starts_with('-'))
        .take(3)
        .collect();
    words.join(" ")
}

/// The row of `element`, an element of a JSON array.
fn element_row(element: &Value) -> PickRow {
    let mut fields: Vec<(String, String)> = match element {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| (key.clone(), field_text(value)))
            .collect(),
        _ => Vec::new(),
    };
    let line = match element {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    fields.push(("line".to_owned(), line));
    PickRow { fields }
}

/// The text of the field that `value` gives.
fn field_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{list_head, parse_rows, PickRow};

    /// The text of the field `name` of each row of `rows`.
    fn fields<'a>(rows: &'a [PickRow], name: &str) -> Vec<Option<&'a str>> {
        rows.iter().map(|row| row.field(name)).collect()
    }

    #[test]
    fn a_json_array_of_objects_gives_a_field_for_each_key() {
        let rows = parse_rows(br#"[{"number":412,"title":"Fix it","draft":false,"base":null}]"#);
        let row = &rows[..];
        assert_eq!(
            (
                fields(row, "number"),
                fields(row, "title"),
                fields(row, "draft"),
                fields(row, "base"),
                fields(row, "line"),
                fields(row, "nope"),
            ),
            (
                vec![Some("412")],
                vec![Some("Fix it")],
                vec![Some("false")],
                vec![Some("")],
                vec![Some(
                    r#"{"base":null,"draft":false,"number":412,"title":"Fix it"}"#
                )],
                vec![None],
            ),
        );
    }

    #[test]
    fn a_json_array_of_strings_gives_line_rows() {
        let rows = parse_rows(br#"["main", "dev", 3]"#);
        assert_eq!(
            fields(&rows, "line"),
            [Some("main"), Some("dev"), Some("3")]
        );
    }

    /// Text that is no JSON array gives a row for each line with text, with
    /// its leading spaces kept.
    #[test]
    fn text_that_is_no_json_array_gives_one_row_for_each_line() {
        assert_eq!(
            [
                fields(&parse_rows(b"* main\n\n  dev\n"), "line"),
                fields(&parse_rows(br#"{"a":1}"#), "line"),
                fields(&parse_rows(b"[1, 2"), "line"),
            ],
            [
                vec![Some("* main"), Some("  dev")],
                vec![Some(r#"{"a":1}"#)],
                vec![Some("[1, 2")],
            ],
        );
    }

    #[test]
    fn odd_input_gives_rows_and_no_panic() {
        let many: String = (0..6_000).map(|at| format!("row {at}\n")).collect();
        assert_eq!(
            (
                fields(&parse_rows(b"a\xffb\n"), "line"),
                parse_rows(b"").len(),
                parse_rows(many.as_bytes()).len(),
            ),
            (vec![Some("a\u{fffd}b")], 0, 5_000),
        );
    }

    #[test]
    fn list_head_stops_at_the_first_dash_word() {
        assert_eq!(
            [
                list_head("gh pr list --json number,title"),
                list_head("kubectl get pods all -n x"),
                list_head("ls"),
                list_head("rg --files -g x"),
            ],
            ["gh pr list", "kubectl get pods", "ls", "rg"],
        );
    }
}
