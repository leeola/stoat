# Stoatty non-cell UI components

## The problem

Stoatty renders modern, IDE-grade UI while keeping authoring as easy as a TUI:
the program emits bytes, the renderer owns pixels. So far every rich feature has
been expressible as an attribute on a cell, a run, or an edge (borders, scaled
glyphs, decorated underlines), or as a floating region that occludes cells
(popovers, status icons). A gutter breaks that mold. A real editor gutter packs,
into roughly two columns, things the cell grid cannot say:

- a line number drawn *smaller* than the body text, vertically centered on its
  row;
- several thin, variable-width color bars (diagnostic severity, git status)
  narrower than a cell;
- a hairline separator a fraction of a cell wide.

Grid text stays integer-scaled and cell-snapped. A scaled VT glyph owns a whole
cell block (`cell_glyph_scale` returns `u8`), glyph origins snap to whole cells
(`glyph_origin` computes `col * width` / `row * height`), and background fills
are whole cells (`BgInstance` is a cell coordinate plus a color). Off the grid,
though, the renderer already draws at a fractional size and a sub-cell position.
A text run carries a `u16` scale in 256ths of a cell and a signed `i16` col/row
in sixteenths (`TextRunCommand`). `shape_char` rasterizes each glyph at that
fractional `f32` scale into its own atlas entry, and `text_run_origin` anchors
the run at a fractional cell, advancing one scaled cell width per glyph and
centering it in its row. Those off-grid text runs, and thin color bars, are the
primitives the gutter is built from.

The floating-overlay layer already points the way out. Overlays anchor at a
signed sub-cell pixel offset, there is a grid-level list beside the cells
(`grid.overlays()`, `grid.icons()`), and the icon pass draws signed-distance
shapes that are not glyphs at all. The gutter is the same kind of thing, one
step further: a component that renders off the grid, at sub-cell precision, and
that must stay aligned to the editor's rows.

## The principle this preserves

The cell grid stays the model and the boundary **for VT text**. Every classic
program still writes into a perfectly uniform grid of identical cells and is
completely unaffected by any of this. What changes is that rich UI no longer has
to be expressible as a cell attribute. It moves into a separate **off-grid
component layer** that the renderer composites over the grid. The renderer
becomes a compositor of uniform surfaces plus their bound components, rather than
a painter of one cell grid.

This keeps the original bargain intact (the program emits a stream; something
else worries about display) and holds the no-reflow line (the grid never
reflows; components are positioned and bound, not flowed).

## The surface model

A **surface** is the unit the renderer composites. Today there is exactly one,
the root grid. A surface owns:

- a uniform cell grid (the existing `Grid`);
- **cell metrics** in physical pixels (the existing `CellMetrics`: cell width,
  cell height, font size);
- a **logical-line layout** (new, see below);
- a set of **off-grid components** bound to it (new).

Everything inside a surface is uniform: one cell size, one grid. Richness lives
in the components layered over it, never in the cells.

### (1) Logical-line layout and integer-cell inline expansions

A surface exposes, per frame, a layout mapping each logical line to the physical
rows it occupies:

```
line_layout: [ (logical_line, start_row, height_in_rows) ]   // height >= 1
```

Most lines have height 1, so the layout is the identity (logical line N at row
N). An **inline expansion** gives a logical line a height greater than 1 -- an
inline diff, a multi-line diagnostic, a blame strip -- and every later line's
`start_row` shifts down by the extra rows. Expansions are **integer-cell**: they
consume whole rows, so the grid stays uniform; the expansion is simply extra
rows allocated to one logical line, not a taller cell. A component that needs to
sit on logical line N looks up its `start_row` and `height` rather than assuming
row == line.

The grid is flat physical rows today (`Grid` is row-major cells; `Terminal`'s
projection copies cells row by row), with no notion of a logical line. The
binding item adds this layout as the model a component's alignment reads. The
representation is a height per logical line; physical `start_row` is the prefix
sum, so an expansion above a line moves it down for free.

### (2) Declaring a component over the APC protocol

Off-grid components are declared with the same `Gstoatty` APC frame mechanism as
borders, popovers, icons, and scroll regions: a namespaced sub-command with a
binary payload. A component declaration carries the surface it binds to (the
root surface today, an explicit id once there are several), the component kind,
and the kind's parameters. It does **not** occupy cells; like the overlay and
icon lists, it is recorded beside the grid and applied each projection, and the
renderer composites it.

Because any terminal that does not recognize the sub-command ignores the whole
APC string, a component degrades to nothing elsewhere -- the program still runs
as a plain VT app, just without the gutter chrome. This is the same additive,
fail-safe contract the rest of the protocol already follows, and it is why the
gutter must remain pure decoration: the line numbers a fallback terminal needs
must still be emitted as ordinary cell text if the program wants them visible
there, with the off-grid gutter as an enhancement on top.

### (3) Binding to live cell metrics and line layout

A component is declared in surface-relative terms (which logical line, which
gutter column, how many rows), never in baked pixels. At render time it reads its
surface's **current** cell metrics and line layout and derives its pixel
geometry from them. Two things change underneath it and must not require
re-declaration:

- **Live font zoom.** `gpu.rs`'s `set_font_size` rederives `CellMetrics` and the
  grid dimensions without telling the program -- the pixel cell size moves while
  the byte stream is unaware. A component that had baked pixel positions would
  drift on every zoom step. Reading the current metrics each frame keeps line
  numbers centered and bars sized correctly through zoom automatically.
- **Inline expansions.** When a line above expands, later lines' `start_row`
  shift. A component bound to a logical line re-reads the layout and re-places,
  so it tracks the editor rather than a stale row.

The binding is therefore "by reference to the surface's current metrics and
layout, evaluated in the renderer each frame," not a value captured at
declaration. This mirrors how the existing passes already consume `CellMetrics`
fresh each frame rather than caching pixel positions.

### (4) The rendering primitives and where they sit

The gutter implies two primitives the cell model cannot express. Both are
declared per the protocol above and bound per (3).

- **Fractional, vertically-centered text run.** Shape a run of glyphs at a
  fractional scale (not the integer `u8` of `shape_char`) and place it at a
  fractional cell position, vertically centered within a target row. This
  generalizes both the integer size in `shape_char` and the integer-cell origin
  in `glyph_origin` to `f32`. It lives in the text pass as its own instance
  stream (the atlas already keys glyphs by rasterized size, so a fractional size
  is just another key), drawn with the grid glyphs.
- **Sub-cell color bars.** Fill thin rectangles at a sub-cell x, width, and
  partial height. `BgInstance` is whole-cell (cell coordinate plus color), so
  this is a new instanced fill -- either a dedicated bar pass or a sub-cell
  extension of the background instance -- carrying pixel-precise x / width / y /
  height within a cell column.

In the `render_into` pass chain (background, decoration, grid text, region text,
cursor, overlays, overlay text, icons), the component primitives compose as a
group after the grid text and alongside the existing off-grid layer (overlays
and icons), each scissored to its surface. A gutter draws over columns the editor
leaves blank, so its bars and numbers sit over the cleared gutter region rather
than over body text. Exact z-order among components follows declaration order,
as the overlay and icon lists already do.

### (5) Staging toward multi-surface compositing

The surface abstraction is chosen so multiple surfaces are an extension, not a
rewrite. Today the renderer composites one surface. Tomorrow it can composite a
list of them -- split panes each a surface, or a nested TUI (a Helix-in-a-box)
that is itself a uniform grid with its **own** cell metrics -- by giving each
surface its own grid, metrics, layout, and components and compositing them in
order. Each surface stays internally uniform; only the composition crosses
surfaces.

The gutter does **not** require any of this: it binds to the single root
surface. Multi-surface is called out only so the surface model is shaped to grow
into it -- per-surface cell metrics rather than one global metric, a surface id
on component declarations, a list of surfaces in the renderer -- without forcing
that work now.

## How the follow-on items map onto this

- **Fractional, vertically-centered text-run primitive** implements (4)'s first
  primitive: fractional sizing in `shape_char`/`cell_glyph_scale` and a
  fractional, vertically-centered origin in `glyph_origin`, plus the protocol
  shape from (2).
- **Sub-cell color-bar primitive** implements (4)'s second primitive: a sub-cell
  instanced fill beside `background.rs`, plus the protocol shape from (2).
- **Surface logical-line -> row+height layout and binding** implements (1) and
  (3): the layout model on the surface and the component binding that reads live
  metrics and layout.

The `example_gutter_app` demo is the first consumer, composing all three to draw
a right-aligned smaller-than-grid line number centered on its row, variable-width
status bars, and a hairline separator, all off-grid while the editor text stays
on the uniform cell grid.
