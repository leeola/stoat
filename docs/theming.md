# Theming

Stoat's colors live in two independent places, because two different programs
draw them:

- The **editor theme** styles everything the stoat editor itself renders --
  text, cursor, selection, gutter, modals, statusline, diffs, syntax
  highlighting. It is written in the `theme NAME { ... }` DSL inside the
  editor's config (`config.stcfg`).
- The **terminal palette** styles what the stoatty terminal draws around and
  underneath its child program: the window background, the 16 ANSI colors any
  program (including a shell) prints with, and the block cursor. It is a
  `[themes.NAME]` table in stoatty's own `config.toml`.

They never overlap. Recoloring the editor does nothing to a shell you run in a
stoatty split, and vice versa. This document covers the editor theme first,
then the terminal palette.

## The editor theme

### Where it lives

The built-in default theme is the
`theme ermine_night inherits default_dark { ... }` block of the embedded
`config.stcfg`. It builds on the `theme default_dark { ... }` block at the top
of that file. Your own overrides go in `~/.config/stoat/config.stcfg` (open it
with the `OpenConfig` command, which creates it from the default if it does not
exist). The active theme is chosen by the `theme` setting --
`on init { theme = ermine_night; }` -- and defaults to `ermine_night`.

You never have to copy the whole default. A `theme` block in your config layers
over the built-in one of the same name field by field, so restating a single
scope is enough (see [Layering](#layering-inheritance-and-switching)).

### Anatomy of a theme block

A theme is a named block holding two kinds of statements: palette `let`s that
name colors, and scope settings that style UI elements.

```
theme default_dark {
    let bg     = "#282c34";
    let accent = "#61afef";

    ui.background.bg = bg;
    ui.border.focused.fg = accent;
    ui.cursor = { modifiers: [reversed] };
}
```

### Palette lets

A `let name = <color>;` binds a name to a color. Later statements refer to it by
that bare name. A `let` may reference an earlier `let`, so palettes build in
layers (a base of raw hex colors, then semantic names on top).

Palette resolution runs in two phases across the whole theme: every `let` is
collected first, then every scope setting resolves against the finished palette.
So redefining a palette name -- even in a later block or a user override --
recolors *every* scope that references it, not just the ones written after the
redefinition. A `let`-to-`let` alias still binds to whatever its referent meant
at the point the alias was written.

### Scope settings

A scope setting styles one UI element, named by a dotted scope path. There are
two equivalent forms:

```
ui.text.fg = text;                                   # dotted-field form
ui.cursor  = { fg: black, bg: text, modifiers: [reversed] };   # map form
```

Use the dotted form to set one field; use the map form to set several at once.
A style scope carries up to three fields:

- `fg` -- foreground color.
- `bg` -- background color.
- `modifiers` -- an array of text attributes.

The `ui.panel.*` and `ui.type.*` scopes take other fields, which
[Panel and type metrics](#panel-and-type-metrics) describes.

### Colors

A color field or a `let` value accepts:

- **A named color**: `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`,
  `black`, `white`, `gray` (or `grey`), `dark_gray`, `light_red`,
  `light_green`, `light_yellow`, `light_blue`, `light_magenta`, `light_cyan`,
  and `reset`. Names are case-insensitive and ignore `_`/`-`, so `dark_gray`,
  `darkgray`, and `DarkGray` are the same.
- **A hex string**: `"#rrggbb"`, quoted, exactly six hex digits.
- **An indexed ANSI color**: `"ansi(N)"`, quoted, `N` from 0 to 255.
- **A palette reference**: a bare name resolves against the palette `let`s
  first, then falls back to the named colors above. An unknown name is an
  error.

### Modifiers

The `modifiers` field takes an array of text attributes: `bold`, `italic`,
`underlined` (or `underline`), `reversed` (or `reverse`), `dim`,
`strikethrough` (or `crossedout`/`crossed`), `slowblink`, `rapidblink`, and
`hidden`.

```
ui.heading = { fg: warning, modifiers: [bold] };
syntax.comment = { fg: muted, modifiers: [italic] };
```

### Panel and type metrics

Two scope groups shape the chrome instead of coloring it. A `ui.panel.<role>`
scope sets the frame of one kind of floating panel:

- `radius` -- the corner radius in logical pixels, a whole number from 0 to
  255.
- `shadow` -- the shadow, one of `none`, `drop`, `tucked`, or `overhang`.
- `inset_x` -- the logical pixels taken off each side edge, a whole number from
  0 to 255.
- `above_pools` -- `true` or `false`, whether the role's panels draw above
  every pooled surface. Each frame in stoat sets this flag itself, so the field
  changes no frame.

A `ui.type.<role>` scope sets one text size in its `scale` field, a whole number
from 1 to 65535 in 256ths of a cell, so 256 fills a cell.

A field that a theme leaves unset takes the Ermine value below, so a theme that
sets none of them draws the Ermine chrome. A value that its field does not hold,
such as `radius: 2.5` or an unknown shadow name, fails the theme load.

```
ui.panel.overlay = { radius: 4, shadow: drop };
ui.type.ui.scale = 240;
```

| Scope              | Ermine value                     |
| ------------------ | -------------------------------- |
| `ui.panel.island`  | radius 6, `none`, inset_x 0      |
| `ui.panel.popout`  | radius 4, `overhang`             |
| `ui.panel.overlay` | radius 6, `drop`, above_pools    |
| `ui.type.title`    | 320                              |
| `ui.type.body`     | 256                              |
| `ui.type.ui`       | 232                              |
| `ui.type.caption`  | 216                              |
| `ui.type.small`    | 208                              |
| `ui.type.mode`     | 208                              |

The island frame is flush with a pane's cells, at inset_x 0. Inside stoatty, a
theme that sets `ui.surface.ground` or `ui.surface.chrome` lays the panes out
one chrome column in from each side of the window. That whole-cell margin
carries the gap around a pane, and an inset hides the outer pixels of the
pane's first and last column.

The popout cards over the status bar draw in the `popout` frame. Every modal
and cursor popup, the key hints box, the docks, and the run pane draw in the
`overlay` frame. Tabs, the status bar, the popout cards, the key hints box,
hover text, and walkthrough labels draw at `ui.type.ui`, gutter numbers at
`ui.type.small`, and modal titles at `ui.type.body`. The `ui.type.caption` step
sits between `ui.type.ui` and `ui.type.small`. It is for 11 px chrome text,
such as section headers, banner rows, and small chips.

## Semantic palette groups: the primary knob

The `default_dark` theme defines a base of raw colors and then a semantic layer
on top of them, and `ermine_night` restates both. The semantic names are the
intended retheming knob -- most scopes reference these, not raw hex:

```
let accent  = cyan;      # highlights, focus, key labels
let primary = blue;      # the normal-mode identity color
let success = green;     # additions, ok states
let warning = yellow;    # attention, prompts, picker matches
let danger  = red;       # errors, deletions
let info    = cyan;      # informational accents
let special = magenta;   # distinctive one-off accents
let muted   = dark_gray; # de-emphasized chrome and borders
let subtle  = "#333842"; # faint background washes
let dim     = gray;      # secondary text
let text    = white;     # primary text
```

Because palette resolution is two-phase, overriding one semantic name in your
config recolors everything that references it. To make every accent teal without
touching a single scope line:

```
# ~/.config/stoat/config.stcfg
theme ermine_night {
    let accent = "#14b8a6";
}
```

That one `let` re-tints the focused border, key labels, active badge, and every
other `accent` scope at once.

## Scope fallback

Scope lookups broaden progressively: a request for `a.b.c` tries `a.b.c`, then
`a.b`, then `a`. This matters most for syntax highlighting, where tree-sitter
emits open-ended capture names: a theme that sets only `syntax.keyword` styles
`syntax.keyword.control` and `syntax.keyword.return` too, until it overrides one
specifically. The same rule lets a scope group share a base style and specialize
a few leaves.

## Scope catalog

The typed UI scopes, by group (syntax scopes are open-ended):

- `ui.background` -- the editor's base background, which every pane's content
  clears to.
- `ui.surface.ground` -- the window ground that every frame blanks to, which
  shows in the window margins and the gaps between panes (background). A theme
  that leaves it unset grounds the window on `ui.surface.chrome`, and a theme
  that sets neither grounds it on `ui.background`.
- `ui.surface.chrome`, `ui.surface.overlay` -- the surface tokens for the window
  chrome and for floating overlays (foreground the stroke, background the
  fill). The background of `ui.surface.chrome` inks the chips that sit on the
  ground, such as the git status chip, and seeds the tints of the badges.
- `ui.surface.raised` -- the fill of a row raised inside a floating box, such
  as the hover popup's footer of goto keys. A theme that leaves it unset
  fills the row with the box's own fill.
- `ui.popover` -- the diagnostic popover's box (background the fill,
  foreground the border). Its header names the diagnostic in the severity
  color and its severity in `ui.text.muted`, over a message in `ui.text.dim`.
  A theme that leaves the background unset fills the box a shade darker than
  the editor background, and one that leaves the foreground unset borders it
  in the severity color.
- `ui.text`, `ui.text.muted`, `ui.text.dim`, `ui.text.disabled` -- body text at
  descending emphasis.
- `ui.cursor`, `ui.cursor.input` -- the block cursor in a buffer and in an input
  field.
- `ui.selection`, `ui.selection.editor`, `ui.selection.reversed` -- selected
  ranges. The editor lifts text under the `ui.selection.editor` wash toward
  white or black when the two are close in brightness, so selected text stays
  legible in a theme that pairs them.
- `ui.search.match` -- the characters a picker's query matched, in a theme
  that sets no `ui.list.match`.
- `ui.list.selected`, `ui.list.match` -- the selected row of every list and
  the characters its query matched. A theme that leaves `ui.list.selected`
  unset paints each list's row with `ui.selection`, or with
  `ui.selection.reversed` in the commits, rebase, and conflict views. A theme
  that leaves `ui.list.match` unset paints the matches with
  `ui.search.match`. Inside stoatty the selected row also carries an accent
  bar at its left edge, in the `ui.border.focused` color.
- `ui.list.current` -- the row of a list that another pane shows, such as the
  file the commits view's diff preview stands on in its files table.
- `ui.chip` -- the chip at the right end of a filter modal's header row, such
  as the file finder's note of what Shift-Tab switches its list to.
- `ui.highlight.read`, `ui.highlight.write` -- symbol-occurrence highlights.
- `ui.border.focused`, `ui.border.inactive` -- pane and modal borders.
- `ui.readonly.label`, `ui.readonly.frame` -- the label and the dashed ring
  a read-only editor pane draws inside stoatty. default_dark leaves both
  unset, so its read-only panes draw neither.
- `ui.pane_picker.badge` -- the digit badge the Pane Picker draws on each
  pane it reaches: the background is the fill, and the foreground is the
  digit and the border. A theme that leaves it unset fills the badge with the
  `ui.statusbar.focused` background and inks it with the `ui.background`
  background.
- `ui.divider` -- the faint track a meter fills over, such as the stat bar of
  a file row in the commits view.
- `ui.modal.help`, `ui.modal.hints`, `ui.modal.palette`, `ui.modal.picker`,
  `ui.modal.run` -- per floating modal, the border hue (foreground) and the box
  surface (background). A modal style with no background keeps the window
  ground. The cursor popups and the small pickers share `ui.modal.hints`.
- `ui.modal.hints.card` -- the which-key hints box. Under stoatty it draws as
  a borderless card docked on the status bar, so only its background shows,
  as the card's fill. In cells its foreground is the border hue. A theme that
  leaves it unset gives the box `ui.modal.hints`.
- `ui.modal.footer` -- the footer row a modal ends in, with its key hints and
  page dots.
- `ui.completion.kind.function`, `ui.completion.kind.constant`,
  `ui.completion.kind.module`, `ui.completion.kind.variable`,
  `ui.completion.kind.keyword` -- the badge that leads a completion row, a
  glyph (foreground) on a tinted cell (background). Methods take the function
  badge, enums, classes, and structs take the constant badge, and fields take
  the variable badge. An item of any other kind leaves the cell blank.
- `ui.prompt`, `ui.key_label`, `ui.heading`, `ui.error`, `ui.message.error` --
  chrome accents.
- `ui.toggle.active`, `ui.toggle.inactive` -- the on and off marks beside a
  toggle's key in the key hints box.
- `ui.hints.submenu` -- the label of a key hints row whose keys open a
  submenu, the row that reads `+name`. Unset, the label takes
  `ui.key_label`.
- `ui.badge.active`, `ui.badge.complete`, `ui.badge.error` -- transient status
  badges.
- `ui.tag.rust`, `ui.tag.markdown`, `ui.tag.config` -- the file-type tag colors
  for Rust, Markdown, and config files.
- `ui.panel.*`, `ui.type.*` -- panel frames and text sizes, set by the fields in
  [Panel and type metrics](#panel-and-type-metrics).
- `ui.walkthrough.focus`, `ui.walkthrough.card`, `ui.walkthrough.marker1`
  through `marker6` -- a walkthrough stop's hand-drawn marks: the focus mark and its
  connector, the narration card and label boxes (foreground the stroke,
  background the fill), and the annotation colors, which cycle through the six
  and color each annotation's label box, its connector, and its code while the
  reader is on it.
- `ui.statusbar.focused`, `ui.statusbar.unfocused`, `ui.mode_label` -- the
  docked status bar and its mode chip.
- `ui.statusbar.separator` -- the hairline along the top of the status bar
  and the tab bar. A theme that leaves it unset draws `ui.border.inactive`.
- `ui.statusbar.unstaged`, `ui.statusbar.staged` -- the status bar's counts of
  the focused file's unstaged and staged hunks.
- `ui.statusbar.clean` -- the status bar's clean mark, which its repo segment
  shows when the repository holds no staged, unstaged, or untracked change.
- `ui.tabbar.toggle.active`, `ui.tabbar.toggle.inactive` -- the `files` and
  `changes` chips at the right of the tab bar while a diff view or a changes
  view has the focus, the chip of the focused view's layout active. A chip
  takes the tab bar's colors where its scope leaves them unset. A chip's
  background fills its box where it differs from the tab bar's.
- `ui.tabbar.name` -- the workspace name that leads the tab bar, in its
  foreground. A theme that leaves it unset draws the name in the text color
  of `ui.tabbar.inactive`.
- `ui.statusline.<mode>` -- the mode chip color per editor mode (`normal`,
  `insert`, `select`, `prompt`, `run`, `commits`, `rebase`, `reword`,
  `conflict`, `review`, `submode`, `default`).
- `diff.added`, `diff.deleted`, `diff.modified`, `diff.moved`, `diff.context`,
  `diff.current_hunk` -- the diff and review views.
- `diff.fold` -- the fill (background) of a changes view's fold row, the row
  that names how many unchanged lines a fold hides. The row's text takes
  `ui.text.muted`. A foreground stripes the fill with a 135 degree hatch in
  that color under stoatty, and fills the cells solid in a plain terminal.
- `diff.fold.button` -- a fold row's Fold Buttons at the row's left: the arrow
  glyph (foreground) on its square box (background). An unset scope takes
  `diff.fold`'s colors.
- `diff.fold.range` -- the foreground of a fold row's line range at the row's
  right, the first and the last line the fold hides.
- `diff.scope.keyword`, `diff.scope.name`, `diff.scope.separator` -- the
  foregrounds of the Scope chip a changes view's fold row centers, the chain
  of items that hold the hunk below the fold, such as `impl Panel` then
  `fn modal_frame`. Each item's keyword and name take the first two. The
  `U+203A` mark between items takes the third, as does the `end of file`
  label. The chip itself takes `diff.fold`'s background.
- `diff.reveal` -- the mark on the rows an expand just opened in the changes
  view: a lane at each side's left edge (foreground) over a tint across the
  row (background). The mark holds for 0.6 s and then fades out over 0.4 s.
- `diff.absent` -- the fill (background) of a split diff row's empty half,
  where the row holds a line on the other side alone: a removed line's live
  half, or an added line's base half. A foreground hatches it as
  `diff.fold`'s does.
- `ui.minimap.shape`, `ui.minimap.shape.comment` -- the grey (foreground) of
  a minimap row that draws the outline of its band of code, for a file whose
  strip rows each stand for many lines, and of a band that is mostly
  comments.
- `ui.minimap.rule` -- the hairline (foreground) a minimap draws above the row
  where a top-level item starts.
- `ui.minimap.rail` -- the rail beside a minimap strip whose file does not fit
  it: the part of the file the strip covers (foreground) on the rail's ground
  (background).
- `diff.sides.base`, `diff.sides.base.name`, `diff.sides.base.label`,
  `diff.sides.target`, `diff.sides.target.name` -- the two halves of a split
  diff view's status row, its text (foreground) on its tint (background), the
  color of the side's name, and the color of the screen label that leads the
  base half. A channel the theme leaves unset tints the status bar toward
  `diff.deleted` on the base half and toward `diff.added` on the target half.
  An unset label takes the foreground of `diff.sides.base` where the theme
  sets that scope, as every scope falls back to its parent, and the base
  name's color where it sets neither.
- `ui.diagnostic.error`, `ui.diagnostic.warning`, `ui.diagnostic.info`,
  `ui.diagnostic.hint` -- LSP diagnostic severities.
- `vcs.conflict.*`, `vcs.commit.*`, `vcs.rebase.*` -- merge conflicts, the
  commit log, and the interactive-rebase todo.
- `vcs.lane.1` to `vcs.lane.6` -- the colors the commit graph's lanes cycle
  through. A lane the theme leaves unset takes a built-in color.
- `vcs.commit.parent`, `vcs.commit.parent.merged` -- the chips that name a
  commit's parents in the commits view's detail header: the first parent,
  then the branches a merge brings in.
- `vcs.commit.row.added`, `vcs.commit.row.removed` -- the backgrounds of the
  lines a commit added and removed, in the commits view's preview. A theme
  that leaves them unset tints no row.
- `vcs.ref.head`, `vcs.ref.remote`, `vcs.ref.tag` -- the badges beside a
  commits list row: HEAD and its branch, a remote-tracking branch, and a tag.
  A local branch's badge takes its lane's color instead. Inside stoatty a
  remote or tag badge also carries a hairline frame, in its color mixed
  toward `ui.background`.
- `vcs.conflict.band`, `vcs.conflict.chip`, `vcs.conflict.ghost`,
  `vcs.conflict.head`, `vcs.conflict.mark` -- the three-way merge view: an
  unresolved chunk's merged rows (background) and their bar (foreground), the
  chip on a chunk's first merged row, the ancestor's text in the merged column,
  the merged column's header cell, and the mark on a conflicted file's tab.
  The `vcs.conflict.ours` and `vcs.conflict.theirs` backgrounds tint the two
  side columns. The ermine_night theme sets these, and default_dark leaves
  them unset.
- `vcs.merge.banner`, `vcs.rebase.banner` -- the banner row of a merge in
  progress and of a rebase paused on a conflict.
- `vcs.status.conflicted`, `vcs.status.staged`, `vcs.status.unstaged`,
  `vcs.status.untracked` -- the section heads of the git status panel.
  `vcs.status.kind.added`, `.modified`, `.deleted`, `.renamed`, `.untracked`,
  and `.unmerged` color the kind letter of each row.
- `chat.*` -- the agent chat panel.
- `syntax.*` -- tree-sitter highlight captures (open-ended, with the fallback
  above).

## Layering, inheritance, and switching

**Layering.** The embedded `config.stcfg` is always the base. A `theme` block in
your user config with the same name overlays it field by field, so you restate
only what you change. A theme whose name exists only in your config resolves too
-- there is no need to redeclare the built-in.

**Inheritance.** A theme may extend another with `inherits`:

```
theme midnight inherits default_dark {
    let accent = "#c678dd";
    ui.background.bg = "#1b1d23";
}
```

The parent's blocks resolve first and the child's `let`s and scopes override
them, propagating through the two-phase palette. Chains resolve to any depth; a
missing parent or an inheritance cycle is a load error, and the theme falls back
to empty.

**Switching at runtime.** The `:SetTheme <name>` command re-resolves and applies
a theme without restarting. Typing `:SetTheme ` and a space opens a completion
list of the loaded theme names. An unknown name keeps the current theme and
reports a message.

## The stoatty terminal palette

stoatty -- the terminal that hosts the editor -- has its own, separate color
config: `stoatty.toml` in the repo root as the built-in default, overlaid by
`~/.config/stoatty/config.toml`. As with the editor config, a user file
overrides only the fields it sets; everything else keeps the default.

The palette is a named `[themes.NAME]` table, selected by the top-level `theme`
setting. The default is `ermine_night`, the name of the editor's default theme
too, which stoatty also exports as `STOAT_THEME` so a child editor follows the
terminal:

```toml
# ~/.config/stoatty/config.toml
theme = "zed"

[themes.zed]
background = "#282c34"
foreground = "#abb2bf"
cursor     = "#74ade8"
black = "#282c34"
red   = "#e06c75"
green = "#98c379"
# ... the rest of the 16 ANSI colors, plus bright_* variants
```

Every value is a `#rrggbb` hex string, and an omitted slot keeps the built-in
default. This palette governs the terminal surface: the window background and
foreground, the block cursor, and the 16 ANSI colors that any child program --
a shell, a pager, or the editor's own ANSI output -- prints with. It has no
effect on the editor theme's semantic scopes, which resolve entirely from
`config.stcfg`.
