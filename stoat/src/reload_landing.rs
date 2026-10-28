//! Where a cursor lands when a reload rewrites the text it sat in.
//!
//! A reload applies the line diff region by region, so a cursor on a line both
//! texts share keeps that line. A cursor inside a changed region has no line to
//! keep, and the edit leaves it at the region's edge. An agent that rewrites a
//! function then throws the cursor out of that function. So the
//! cursor's seat is read against the innermost item that held it, such as a
//! function, and the same item in the new text takes the cursor at the same
//! row offset.
//!
//! Every function here is pure, so the tests run without an editor.

use stoat_language::{extract_symbols, parse, Language, SymbolDef, SymbolKind};
use stoat_text::{Point, Rope};

/// Where a cursor sat relative to the innermost item that held it.
///
/// The name, the kind, and the container find the item again in the new text,
/// since a reload moves an item's offsets but rarely its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ItemSeat {
    name: String,
    kind: SymbolKind,
    container: Vec<String>,
    /// The cursor's row minus the item's first row.
    rows_into: u32,
    column: u32,
}

/// The seat of the cursor at `offset` in `rope`, against the smallest item of
/// `defs` that holds it, or `None` when no item holds it.
pub(crate) fn seat_at(defs: &[SymbolDef], rope: &Rope, offset: usize) -> Option<ItemSeat> {
    let def = defs
        .iter()
        .filter(|def| def.def_range.contains(&offset))
        .min_by_key(|def| def.def_range.len())?;
    let start = rope.offset_to_point(def.def_range.start);
    let at = rope.offset_to_point(offset);
    Some(ItemSeat {
        name: def.name.clone(),
        kind: def.kind,
        container: def.container.clone(),
        rows_into: at.row - start.row,
        column: at.column,
    })
}

/// The offset in `rope` that `seat` names in the item of `defs` with its name,
/// kind, and container, or `None` when no item matches.
///
/// Of several matches, the one that starts nearest to `near` wins. The row
/// stops at the item's last row and the column at that line's end, so a
/// shorter item keeps the cursor inside it.
pub(crate) fn offset_for_seat(
    defs: &[SymbolDef],
    rope: &Rope,
    seat: &ItemSeat,
    near: usize,
) -> Option<usize> {
    let def = defs
        .iter()
        .filter(|def| {
            def.name == seat.name && def.kind == seat.kind && def.container == seat.container
        })
        .min_by_key(|def| def.def_range.start.abs_diff(near))?;
    let first = rope.offset_to_point(def.def_range.start).row;
    let last_byte = def.def_range.end.saturating_sub(1).max(def.def_range.start);
    let last = rope.offset_to_point(last_byte).row;
    let row = (first + seat.rows_into).min(last);
    let column = seat.column.min(rope.line_len(row));
    Some(rope.point_to_offset(Point::new(row, column)))
}

/// The items `language`'s outline query finds in `text`, with the rope their
/// ranges index.
///
/// `None` when the language has no outline query or the text does not parse.
pub(crate) fn symbols_of(language: &Language, text: &str) -> Option<(Rope, Vec<SymbolDef>)> {
    let query = language.outline_query()?;
    let tree = parse(language, text, None)?;
    let rope = Rope::from(text);
    let defs = extract_symbols(query, tree.root_node(), &rope, None);
    Some((rope, defs))
}

#[cfg(test)]
mod tests {
    use super::{offset_for_seat, seat_at, symbols_of};
    use std::path::Path;
    use stoat_language::LanguageRegistry;
    use stoat_text::Point;

    /// Where a cursor at `at` in Rust text `old` lands in `new`.
    fn land(old: &str, at: Point, new: &str) -> Option<Point> {
        let language = LanguageRegistry::standard()
            .for_path(Path::new("a.rs"))
            .expect("a Rust language");
        let (old_rope, old_defs) = symbols_of(&language, old).expect("old text parses");
        let (new_rope, new_defs) = symbols_of(&language, new).expect("new text parses");
        let seat = seat_at(&old_defs, &old_rope, old_rope.point_to_offset(at))?;
        let offset = offset_for_seat(&new_defs, &new_rope, &seat, 0)?;
        Some(new_rope.offset_to_point(offset))
    }

    #[test]
    fn a_cursor_keeps_its_row_in_a_rewritten_function() {
        let old = "fn a() {\n    1\n}\n\nfn b() {\n    let x = 1;\n    x\n}\n";
        let new = "fn a() {\n    1\n    2\n    3\n    4\n}\n\nfn b() {\n    let y = 2;\n    y\n}\n";
        assert_eq!(
            land(old, Point::new(6, 4), new),
            Some(Point::new(9, 4)),
            "two rows into fn b, below the three lines fn a grew",
        );
    }

    #[test]
    fn a_cursor_follows_its_function_below_another() {
        let old = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        let new = "fn b() {\n    2\n}\n\nfn a() {\n    1\n}\n";
        assert_eq!(land(old, Point::new(1, 4), new), Some(Point::new(5, 4)));
    }

    #[test]
    fn a_cursor_in_an_item_with_no_counterpart_lands_nowhere() {
        let old = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        let new = "fn a() {\n    1\n}\n\nfn c() {\n    2\n}\n";
        assert_eq!(land(old, Point::new(5, 4), new), None);
    }

    #[test]
    fn two_methods_of_one_name_resolve_by_their_container() {
        let old = "impl A {\n    fn go() {\n        1\n    }\n}\n\nimpl B {\n    fn go() {\n        2\n    }\n}\n";
        let new = "impl B {\n    fn go() {\n        2\n    }\n}\n\nimpl A {\n    fn go() {\n        1\n    }\n}\n";
        assert_eq!(land(old, Point::new(8, 8), new), Some(Point::new(2, 8)));
    }
}
