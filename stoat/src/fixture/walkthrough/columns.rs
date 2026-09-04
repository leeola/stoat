//! The `walkthrough-columns` fixture, a message table whose ranges start past
//! bytes that are not cells.
//!
//! A walkthrough range stores 1-based byte columns, and `measure_range` hands
//! that column to the rope, whose point column is also bytes. The offset it
//! returns then becomes a screen cell. Every other fixture in the family is
//! ASCII indented with spaces, so the byte column, the character index, and the
//! screen column are one number and the three steps of that mapping are never
//! told apart.
//!
//! Here they differ at every stop. The table is indented with real tab
//! characters, and its greetings are accented, CJK, and supplementary-plane
//! text, so a mark placed a byte short or a cell short lands somewhere the
//! reader sees.
//!
//! The last stop is about the boxes rather than the marks. `label_size` and
//! `card_width` both measure by counting characters, so a label written in a
//! script whose glyphs take two cells is sized for half the room it needs.

use crate::{
    fixture::{FixtureError, FixtureRepo},
    walkthrough::Walkthrough,
};
use std::path::Path;

const CARGO: &str = r#"[package]
name = "fixture-walkthrough-columns"
version = "0.1.0"
edition = "2021"

[workspace]
"#;

const MAIN: &str = r#"mod i18n;

fn main() {
    for locale in ["en", "fr", "ja", "ja-ext"] {
        println!("{locale}: {}", i18n::greeting(locale));
    }
}
"#;

/// The message table, indented with tab characters rather than spaces.
///
/// The tabs are literal, and rustfmt leaves the contents of a string alone, so
/// they survive a format pass. They are one byte and one character each, and
/// several cells on screen, which is the difference the first stop is about.
///
/// The last arm carries a supplementary-plane ideograph, four bytes and two
/// cells, which is the widest the two counts pull apart anywhere in the table.
const I18N: &str = "/// The greeting each locale gets, by locale tag.
///
/// One arm per locale, and no fallback beyond the last, so a locale nobody
/// wrote a greeting for reads as the plain English one.
pub fn greeting(locale: &str) -> &'static str {
\tmatch locale {
\t\t\"en\" => \"hello world\",
\t\t\"fr\" => \"café crème\",
\t\t\"ja\" => \"こんにちは世界\",
\t\t\"ja-ext\" => \"𠮷 stoat\",
\t\t_ => \"hello\",
\t}
}
";

const NARRATION_TAB: &str = "\
Two tabs stand before this arm. The store counts them as two bytes, and the
screen draws them as several cells each.

The annotation names a span whose column is measured in bytes, so the mark
around it is right only if the tabs are expanded on the way to the screen.
";

const NARRATION_ACCENTS: &str = "\
The word this annotation names starts after an accented letter, which is two
bytes and one cell.

A mark placed at the byte column without the conversion lands one cell to the
right of the word.
";

const NARRATION_CJK: &str = "\
Every character in this greeting is three bytes and two cells, so the byte
column and the screen column pull apart in both directions at once.

The annotation names the last two characters. Its mark is four cells wide,
over six bytes.
";

const NARRATION_WIDEST: &str = "\
The ideograph before this word is four bytes and two cells, the widest gap
between the two counts this table holds.

Everything after it on the line is offset by three cells against its byte
column.
";

/// Labels for the last stop, written in a script whose glyphs take two cells.
///
/// Each is short in characters and twice that on screen, which is what a box
/// sized by `chars().count()` has to hold.
const WIDE_LABELS: [&str; 3] = ["フランス語", "日本語", "拡張漢字"];

/// The last stop's narration, in the same script as its labels, so the card is
/// measured the way the label boxes are.
const NARRATION_WIDE: &str = "\
このステップのラベルは日本語で書かれています.
文字数は少なく, 画面上の幅はその二倍です.

ラベルの箱もカードの幅も文字数で決まるので,
二セル分の文字が並ぶと箱からはみ出します.
";

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let json = super::tour_json(&build());

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", CARGO),
            ("src/main.rs", MAIN),
            ("src/i18n.rs", I18N),
            (".stoat/walkthroughs/tour.json", &json),
        ],
    )?;
    Ok(())
}

/// The five-stop tour the `walkthrough-columns` fixture commits.
fn build() -> Walkthrough {
    let mut tour = Walkthrough::new(
        "tour".to_string(),
        "Bytes, characters, and cells".to_string(),
        None,
    );

    for (title, narration, arm, span, label) in [
        (
            "After a tab",
            NARRATION_TAB,
            "\"en\" =>",
            "\"hello world\"",
            "two tabs, two bytes, eight cells",
        ),
        (
            "After accents",
            NARRATION_ACCENTS,
            "\"fr\" =>",
            "crème",
            "one byte past where the characters end",
        ),
        (
            "After CJK",
            NARRATION_CJK,
            "\"ja\" =>",
            "世界",
            "six bytes, four cells",
        ),
        (
            "After four bytes",
            NARRATION_WIDEST,
            "\"ja-ext\" =>",
            "stoat",
            "four bytes and two cells to its left",
        ),
    ] {
        let stop = tour
            .add_stop(
                Some(title.to_string()),
                narration.to_string(),
                super::location("src/i18n.rs", I18N, super::line_of(I18N, arm)),
                None,
            )
            .expect("appending a stop cannot fail")
            .id
            .clone();
        super::annotate(
            &mut tour,
            &stop,
            None,
            I18N,
            super::span_of(I18N, span),
            label,
            "",
        );
    }

    let wide = tour
        .add_stop(
            Some("Wide labels".to_string()),
            NARRATION_WIDE.to_string(),
            super::location("src/i18n.rs", I18N, super::line_of(I18N, "match locale {")),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    for (span, label) in ["café crème", "こんにちは世界", "𠮷 stoat"]
        .into_iter()
        .zip(WIDE_LABELS)
    {
        super::annotate(
            &mut tour,
            &wide,
            None,
            I18N,
            super::span_of(I18N, span),
            label,
            "",
        );
    }

    tour
}

#[cfg(test)]
mod tests {
    use super::I18N;
    use crate::{
        host::LocalFs,
        walkthrough::{self, store, Stop},
    };

    /// The lowest code point counted as a two-cell glyph here.
    ///
    /// Everything the wide labels are written in sits above it, and every
    /// character the narrower stops name sits below, so the two groups are told
    /// apart without a width table.
    const WIDE_FLOOR: char = '\u{2fff}';

    /// The tour is built from ranges derived out of the source consts, so a
    /// const gaining a line must not leave a range pointing at the wrong code.
    /// `validate` catches that, and this runs it against the materialized
    /// repository rather than against the builder's own idea of the text.
    #[test]
    fn columns_tour_validates_against_the_committed_sources() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();

        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");
        let drift = walkthrough::validate(&tour, &store::workspace_reader(&LocalFs, dir.path()));
        assert_eq!(
            drift,
            Vec::new(),
            "every range in the tour still covers what it captured",
        );
    }

    /// A range whose byte column equals its character column stages nothing,
    /// because the mapping this fixture is about only shows where the two
    /// disagree. These pin that they disagree, so an edit that flattens the
    /// table to ASCII leaves a tour that still validates and stages nothing.
    #[test]
    fn columns_tour_ranges_start_past_multibyte_bytes() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");

        assert_eq!(tour.stops.len(), 5, "five stops");

        assert!(
            line_of(&tour.stops[0]).starts_with('\t'),
            "the first stop's line is indented with tabs, which the store counts \
             as one byte each and the screen draws as several cells",
        );

        for stop in &tour.stops[1..4] {
            let line = line_of(stop);
            let annotation = &stop.annotations[0];
            let byte = line
                .find(annotation.snippet.as_str())
                .expect("the annotation was captured from this line");

            assert!(
                byte > line[..byte].chars().count(),
                "{:?} starts at byte {byte} of {line:?}, past its character index, \
                 so the stored column is not a character count",
                annotation.snippet,
            );
        }

        assert_eq!(
            tour.stops[4]
                .annotations
                .iter()
                .filter(|annotation| annotation.label.chars().any(|glyph| glyph > WIDE_FLOOR))
                .count(),
            3,
            "every label on the last stop is written in two-cell glyphs",
        );
    }

    /// The line of `I18N` a stop's annotation sits on.
    fn line_of(stop: &Stop) -> &'static str {
        let at = stop.annotations[0].range.start.line as usize;
        I18N.lines()
            .nth(at - 1)
            .expect("the tour's ranges are derived from I18N")
    }
}
