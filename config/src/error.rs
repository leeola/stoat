use ariadne::{Color, Label, Report, ReportKind, Source};
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub span: Range<usize>,
    pub message: String,
    pub label: Option<String>,
}

impl ParseError {
    pub fn new(span: Range<usize>, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
            label: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

pub fn format_errors(source: &str, errors: &[ParseError]) -> String {
    let mut output = Vec::new();

    for error in errors {
        let mut report =
            Report::build(ReportKind::Error, (), error.span.start).with_message(&error.message);

        let mut label = Label::new(error.span.clone()).with_color(Color::Red);
        if let Some(ref label_text) = error.label {
            label = label.with_message(label_text);
        }
        report = report.with_label(label);

        report
            .finish()
            .write(Source::from(source), &mut output)
            .ok();
    }

    String::from_utf8(output).unwrap_or_default()
}
