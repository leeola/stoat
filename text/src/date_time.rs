//! Arithmetic on dates and times written as text.
//!
//! A reader incrementing a timestamp in a file wants the next day or the next
//! minute, not the next integer, and wants it written the way they wrote it.
//! So each recognized form is a parse and a format description at once, and the
//! text goes back out through the description it came in on.

use time::{
    format_description::BorrowedFormatItem, macros::format_description, Date, Duration,
    PrimitiveDateTime, Time,
};

/// Which fields a written form carries, and so what the arithmetic moves.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Shape {
    /// A date and a time. Moves by minutes, leaving the seconds alone.
    DateTime,
    /// A date alone. Moves by days.
    Date,
    /// A time alone. Moves by minutes and wraps at midnight.
    Time,
}

/// A written form, as the description that both reads and writes it.
struct DateFormat {
    items: &'static [BorrowedFormatItem<'static>],
    shape: Shape,
}

/// The forms recognized, tried in order.
///
/// Order is what settles an ambiguity. A form carrying a time comes before the
/// bare date that opens it, so `2021-11-24 07:12` is not read as `2021-11-24`
/// with text left over.
static FORMATS: &[DateFormat] = &[
    DateFormat {
        items: format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
        shape: Shape::DateTime,
    },
    DateFormat {
        items: format_description!("[year]/[month]/[day] [hour]:[minute]:[second]"),
        shape: Shape::DateTime,
    },
    DateFormat {
        items: format_description!("[year]-[month]-[day] [hour]:[minute]"),
        shape: Shape::DateTime,
    },
    DateFormat {
        items: format_description!("[year]/[month]/[day] [hour]:[minute]"),
        shape: Shape::DateTime,
    },
    DateFormat {
        items: format_description!("[year]-[month]-[day]"),
        shape: Shape::Date,
    },
    DateFormat {
        items: format_description!("[year]/[month]/[day]"),
        shape: Shape::Date,
    },
    DateFormat {
        items: format_description!("[weekday repr:short] [month repr:short] [day] [year]"),
        shape: Shape::Date,
    },
    DateFormat {
        items: format_description!("[day]-[month repr:short]-[year]"),
        shape: Shape::Date,
    },
    DateFormat {
        items: format_description!("[year] [month repr:short] [day]"),
        shape: Shape::Date,
    },
    DateFormat {
        items: format_description!("[month repr:short] [day], [year]"),
        shape: Shape::Date,
    },
    // The uppercase period comes first because only an uppercase description
    // honors `case_sensitive`. A lowercase one accepts either case, so ahead of
    // these it claims `7:21 AM` and writes it back lowercase.
    DateFormat {
        items: format_description!(
            "[hour repr:12 padding:none]:[minute]:[second] [period case:upper case_sensitive:true]"
        ),
        shape: Shape::Time,
    },
    DateFormat {
        items: format_description!(
            "[hour repr:12 padding:none]:[minute] [period case:upper case_sensitive:true]"
        ),
        shape: Shape::Time,
    },
    DateFormat {
        items: format_description!(
            "[hour repr:12 padding:none]:[minute]:[second] [period case:lower]"
        ),
        shape: Shape::Time,
    },
    DateFormat {
        items: format_description!("[hour repr:12 padding:none]:[minute] [period case:lower]"),
        shape: Shape::Time,
    },
    DateFormat {
        items: format_description!("[hour]:[minute]:[second]"),
        shape: Shape::Time,
    },
    DateFormat {
        items: format_description!("[hour]:[minute]"),
        shape: Shape::Time,
    },
];

/// Returns `text` with `amount` added to the date or time it spells, or
/// [`None`] when it spells neither.
///
/// The whole of `text` must be the timestamp. A caller hands over the text a
/// selection covers, so a selection holding anything else answers `None`.
///
/// What `amount` counts follows what the text carries. A date alone moves by
/// days. Anything carrying a time moves by minutes, so a full timestamp keeps
/// its seconds. A time alone wraps at midnight rather than carrying into a day
/// it does not name.
///
/// The written form survives the arithmetic. One description both reads and
/// writes a form, so separators, field widths, month and weekday spellings,
/// and am/pm case all come back as they went in.
pub fn date_time_increment(text: &str, amount: i64) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    FORMATS.iter().find_map(|format| match format.shape {
        Shape::DateTime => PrimitiveDateTime::parse(text, format.items)
            .ok()?
            .checked_add(minutes(amount)?)?
            .format(format.items)
            .ok(),
        Shape::Date => Date::parse(text, format.items)
            .ok()?
            .checked_add(days(amount)?)?
            .format(format.items)
            .ok(),
        // Adding to a Time wraps, which is what keeps 23:59 a time rather than
        // making it a date the text never carried.
        Shape::Time => (Time::parse(text, format.items).ok()? + minutes(amount)?)
            .format(format.items)
            .ok(),
    })
}

/// A [`Duration`] of `amount` minutes, or [`None`] when that many minutes do
/// not fit in seconds.
fn minutes(amount: i64) -> Option<Duration> {
    Some(Duration::seconds(amount.checked_mul(60)?))
}

/// A [`Duration`] of `amount` days, or [`None`] when that many days do not fit
/// in seconds.
fn days(amount: i64) -> Option<Duration> {
    Some(Duration::seconds(amount.checked_mul(86_400)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_a_date_by_days_and_a_time_by_minutes() {
        let cases = [
            ("2020-02-28", 1, "2020-02-29"),
            ("2020-02-29", 1, "2020-03-01"),
            ("2020-01-31", 1, "2020-02-01"),
            ("2020-01-20", 1, "2020-01-21"),
            ("2021-01-01", -1, "2020-12-31"),
            ("2021-01-31", -2, "2021-01-29"),
            ("2021-02-28", 1, "2021-03-01"),
            ("2021-03-01", -1, "2021-02-28"),
            ("2020-02-29", -1, "2020-02-28"),
            ("2020-02-20", -1, "2020-02-19"),
            ("1980/12/21", 100, "1981/03/31"),
            ("1980/12/21", -100, "1980/09/12"),
            ("1980/12/21", 1000, "1983/09/17"),
            ("1980/12/21", -1000, "1978/03/27"),
            ("2021-11-24 07:12:23", 1, "2021-11-24 07:13:23"),
            ("2021-11-24 07:12", 1, "2021-11-24 07:13"),
            ("Wed Nov 24 2021", 1, "Thu Nov 25 2021"),
            ("24-Nov-2021", 1, "25-Nov-2021"),
            ("2021 Nov 24", 1, "2021 Nov 25"),
            ("Nov 24, 2021", 1, "Nov 25, 2021"),
            ("7:21:53 am", 1, "7:22:53 am"),
            ("7:21:53 AM", 1, "7:22:53 AM"),
            ("7:21 am", 1, "7:22 am"),
            ("23:24:23", 1, "23:25:23"),
            ("23:24", 1, "23:25"),
            ("23:59", 1, "00:00"),
            ("23:59:59", 1, "00:00:59"),
        ];

        for (original, amount, expected) in cases {
            assert_eq!(
                date_time_increment(original, amount).as_deref(),
                Some(expected),
                "{original} by {amount}",
            );
        }
    }

    #[test]
    fn rejects_text_that_spells_no_timestamp() {
        let cases = [
            "0000-00-00",
            "1980-2-21",
            "1980-12-1",
            "12345",
            "2020-02-30",
            "1999-12-32",
            "19-12-32",
            "1-2-3",
            "0000/00/00",
            "1980/2/21",
            "1980/12/1",
            "2020/02/30",
            "1999/12/32",
            "19/12/32",
            "1/2/3",
            "123:456:789",
            "11:61",
            "2021-55-12 08:12:54",
            "",
        ];

        for invalid in cases {
            assert_eq!(date_time_increment(invalid, 1), None, "{invalid}");
        }
    }
}
