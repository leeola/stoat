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

The built-in default theme is the `theme default_dark { ... }` block at the top
of the embedded `config.stcfg`. Your own overrides go in
`~/.config/stoat/config.stcfg` (open it with the `OpenConfig` command, which
creates it from the default if it does not exist). The active theme is chosen by
the `theme` setting -- `on init { theme = default_dark; }` -- and defaults to
`default_dark`.

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
Each scope carries up to three fields:

- `fg` -- foreground color.
- `bg` -- background color.
- `modifiers` -- an array of text attributes.

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

## Semantic palette groups: the primary knob

The default theme defines a base of raw colors and then a semantic layer on top
of them. The semantic names are the intended retheming knob -- most scopes
reference these, not raw hex:

```
let accent  = cyan;      # highlights, focus, key labels
let primary = blue;      # the normal-mode identity color
let success = green;     # additions, ok states
let warning = yellow;    # attention, prompts, search matches
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
theme default_dark {
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

- `ui.background` -- the editor's base background.
- `ui.text`, `ui.text.muted`, `ui.text.dim`, `ui.text.disabled` -- body text at
  descending emphasis.
- `ui.cursor`, `ui.cursor.input` -- the block cursor in a buffer and in an input
  field.
- `ui.selection`, `ui.selection.editor`, `ui.selection.reversed` -- selected
  ranges.
- `ui.search.match` -- search hits.
- `ui.highlight.read`, `ui.highlight.write` -- symbol-occurrence highlights.
- `ui.border.focused`, `ui.border.inactive` -- pane and modal borders.
- `ui.modal.help`, `ui.modal.hints`, `ui.modal.palette`, `ui.modal.picker`,
  `ui.modal.run` -- the border hue per floating modal.
- `ui.prompt`, `ui.key_label`, `ui.heading`, `ui.error`, `ui.message.error` --
  chrome accents.
- `ui.badge.active`, `ui.badge.complete`, `ui.badge.error` -- transient status
  badges.
- `ui.statusbar.focused`, `ui.statusbar.unfocused`, `ui.mode_label` -- the
  docked status bar and its mode chip.
- `ui.statusline.<mode>` -- the mode chip color per editor mode (`normal`,
  `insert`, `select`, `prompt`, `run`, `commits`, `rebase`, `reword`,
  `conflict`, `review`, `submode`, `default`).
- `diff.added`, `diff.deleted`, `diff.modified`, `diff.moved`, `diff.context`,
  `diff.current_hunk` -- the diff and review views.
- `ui.diagnostic.error`, `ui.diagnostic.warning`, `ui.diagnostic.info`,
  `ui.diagnostic.hint` -- LSP diagnostic severities.
- `vcs.conflict.*`, `vcs.commit.*`, `vcs.rebase.*` -- merge conflicts, the
  commit log, and the interactive-rebase todo.
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
setting (default `zed`):

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
