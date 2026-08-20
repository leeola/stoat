//! The iTerm2 inline-image escape, `OSC 1337 ; File = <k=v>... : <base64> ST`.
//!
//! The other protocol image clients speak. Where a Kitty frame separates
//! transmitting an image from placing it, this sends one escape carrying both:
//! the pixels and how large to draw them, at the cursor, once. There is no id
//! to name it by afterward and no way to delete it, so a terminal has nothing
//! to remember beyond the placement itself.
//!
//! The dimensions are the interesting part. A client states them in cells, in
//! pixels, as a percentage of the screen, or leaves them to the image, and a
//! terminal has to answer all four.

/// How large to draw one side of an image.
///
/// The unit rides with the number because the same digits mean different sizes:
/// `40` is forty cells and `40px` is forty pixels, which on an ordinary cell is
/// a factor of eight apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dimension {
    /// A count of cells.
    Cells(u32),
    /// A count of pixels, which the terminal divides by its cell size.
    Pixels(u32),
    /// A percentage of the screen's own width or height.
    Percent(u32),
    /// Whatever the image itself implies, which is its pixels over the cell
    /// size.
    Auto,
}

/// A parsed inline-image escape.
///
/// The payload stays base64 for the same reason a Kitty frame's does: the
/// terminal decodes once, and nothing here needs the bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ItermFile {
    /// The client's name for the file, decoded from its base64. Absent when the
    /// client sent none or sent one that is not valid base64.
    pub name: Option<String>,
    /// The byte count the client claims, which a terminal offering downloads
    /// would show in its progress. Nothing here reads it.
    pub size: Option<u64>,
    pub width: Dimension,
    pub height: Dimension,
    /// Whether a dimension the client left out follows the image's aspect
    /// ratio, and a box it gave is fitted inside rather than filled. On by
    /// default, so an image sized by width alone is not also stretched.
    pub preserve_aspect_ratio: bool,
    /// Whether to draw the image rather than offer it as a download. A terminal
    /// with no downloads has nothing to do with a file that says otherwise.
    pub inline: bool,
    /// Whether the cursor stays where it was rather than moving past the image.
    pub do_not_move_cursor: bool,
    /// The image, still base64.
    pub payload: Vec<u8>,
}

/// Parse an `OSC 1337` payload, or `None` if it is not an inline-image escape.
///
/// `payload` is what follows the code's `;`, so it opens with `File=`. Any other
/// 1337 escape belongs to a feature this terminal does not have, and returning
/// nothing is how it declines one.
///
/// An unknown key is ignored rather than failing the escape, which is what lets
/// a client written against a later iTerm2 still draw here.
pub fn parse_file(payload: &[u8]) -> Option<ItermFile> {
    let text = std::str::from_utf8(payload).ok()?;
    let rest = text
        .strip_prefix("File=")
        .or_else(|| text.strip_prefix("File"))?;

    // The arguments end at the first colon, which the base64 that follows never
    // contains. A colon inside an argument would be a value no key defines.
    let (args, data) = match rest.split_once(':') {
        Some((args, data)) => (args, data),
        None => (rest, ""),
    };

    let mut file = ItermFile {
        name: None,
        size: None,
        width: Dimension::Auto,
        height: Dimension::Auto,
        preserve_aspect_ratio: true,
        inline: false,
        do_not_move_cursor: false,
        payload: data.as_bytes().to_vec(),
    };

    for pair in args.split(';').filter(|pair| !pair.is_empty()) {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "name" => file.name = decode_name(value),
            "size" => file.size = value.parse().ok(),
            "width" => file.width = parse_dimension(value)?,
            "height" => file.height = parse_dimension(value)?,
            "preserveAspectRatio" => file.preserve_aspect_ratio = value != "0",
            "inline" => file.inline = value != "0",
            "doNotMoveCursor" => file.do_not_move_cursor = value != "0",
            _ => {},
        }
    }

    Some(file)
}

/// Read one dimension value, or `None` when it is not one of the four forms.
///
/// A malformed dimension fails the whole escape rather than falling back to
/// automatic, because a client that meant a size and got the image's own would
/// see a wrong picture rather than none, and the second is easier to diagnose.
fn parse_dimension(value: &str) -> Option<Dimension> {
    if value == "auto" {
        return Some(Dimension::Auto);
    }
    if let Some(pixels) = value.strip_suffix("px") {
        return pixels.parse().ok().map(Dimension::Pixels);
    }
    if let Some(percent) = value.strip_suffix('%') {
        return percent.parse().ok().map(Dimension::Percent);
    }
    value.parse().ok().map(Dimension::Cells)
}

/// Decode a base64 filename, keeping only one that is valid text.
///
/// The name reaches nothing that acts on it, so a client sending something
/// unreadable loses the name rather than the image.
fn decode_name(value: &str) -> Option<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::{parse_file, Dimension, ItermFile};
    use base64::Engine;

    fn payload(args: &str, data: &str) -> Vec<u8> {
        format!("File={args}:{data}").into_bytes()
    }

    fn base64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    /// An escape naming nothing still has to draw, so every key carries the
    /// default a client that omitted it expects.
    #[test]
    fn an_escape_naming_no_keys_takes_the_defaults() {
        let parsed = parse_file(&payload("", "aW1n")).expect("parses");

        assert_eq!(
            parsed,
            ItermFile {
                name: None,
                size: None,
                width: Dimension::Auto,
                height: Dimension::Auto,
                preserve_aspect_ratio: true,
                inline: false,
                do_not_move_cursor: false,
                payload: b"aW1n".to_vec(),
            },
        );
    }

    #[test]
    fn every_key_reaches_its_field() {
        let args = format!(
            "name={};size=1234;width=40;height=20px;preserveAspectRatio=0;inline=1;doNotMoveCursor=1",
            base64("cat.png"),
        );
        let parsed = parse_file(&payload(&args, "cGF5")).expect("parses");

        assert_eq!(
            parsed,
            ItermFile {
                name: Some("cat.png".to_owned()),
                size: Some(1234),
                width: Dimension::Cells(40),
                height: Dimension::Pixels(20),
                preserve_aspect_ratio: false,
                inline: true,
                do_not_move_cursor: true,
                payload: b"cGF5".to_vec(),
            },
        );
    }

    /// The same digits mean different sizes depending on the suffix, so the
    /// unit has to survive parsing.
    #[test]
    fn a_dimension_carries_the_unit_its_suffix_names() {
        let width = |value: &str| {
            parse_file(&payload(&format!("width={value}"), "x")).map(|file| file.width)
        };

        assert_eq!(width("40"), Some(Dimension::Cells(40)));
        assert_eq!(width("40px"), Some(Dimension::Pixels(40)));
        assert_eq!(width("40%"), Some(Dimension::Percent(40)));
        assert_eq!(width("auto"), Some(Dimension::Auto));
    }

    /// A client that meant a size and silently got the image's own sees a wrong
    /// picture, where one that sees nothing knows to look at what it sent.
    #[test]
    fn a_malformed_dimension_fails_the_escape() {
        assert_eq!(parse_file(&payload("width=wide", "x")), None);
        assert_eq!(parse_file(&payload("height=12pt", "x")), None);
    }

    /// A client written against a later iTerm2 still draws here, so a key this
    /// build never heard of is skipped rather than failing the escape.
    #[test]
    fn an_unknown_key_is_ignored() {
        let parsed = parse_file(&payload("width=10;wobble=3;height=5", "x")).expect("parses");

        assert_eq!(
            (parsed.width, parsed.height),
            (Dimension::Cells(10), Dimension::Cells(5)),
            "the keys around it still land",
        );
    }

    /// Every other 1337 escape belongs to a feature this terminal does not
    /// have, and it must not be read as an image.
    #[test]
    fn another_1337_escape_is_not_an_inline_image() {
        assert_eq!(parse_file(b"CurrentDir=/tmp"), None);
        assert_eq!(parse_file(b"SetMark"), None);
        assert_eq!(parse_file(b"RemoteHost=me@here"), None);
    }

    /// The name is decoration a client may send badly, and losing it is a
    /// better outcome than losing the image with it.
    #[test]
    fn an_unreadable_name_is_dropped_rather_than_failing_the_escape() {
        let parsed = parse_file(&payload("name=not!base64!;width=4", "x")).expect("parses");

        assert_eq!((parsed.name, parsed.width), (None, Dimension::Cells(4)));
    }

    /// The arguments end at the first colon, and base64 never contains one, so
    /// the split cannot cut into the image.
    #[test]
    fn the_payload_is_everything_past_the_first_colon() {
        let parsed = parse_file(&payload("width=2", "AAAA/+==")).expect("parses");

        assert_eq!(parsed.payload, b"AAAA/+==");
    }
}
