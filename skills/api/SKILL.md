---
name: api
description: Read and drive the stoat editor the user has open through the `stoat api` CLI, which reads what the user sees (focus, selection, buffers, layout), shows them a place (goto, open), runs actions and types keys (act, keys), asks the language servers (hover, diagnostics), and follows the view (watch). Use it whenever a task touches the editor the user has open.
user_invocable: true
---

# api

`stoat api` reads and drives a live stoat session from the shell. Use it to
see what the user sees, to show the user a place in the code, and to act in
the editor beside them.

Each example reply below is abridged and spread over several lines. The CLI
prints each reply as one line of JSON. Read a reply by its keys.

## The session

Each verb reaches one live session. A session is one workspace of a running
stoat, with a root directory and a hex uid.

- `--session <ID>` picks a session by its uid or by its workspace name. A name
  matches without regard to case. The flag goes before or after the verb.
- Without the flag, a verb that runs in the agent pane of a session reaches
  that session. `STOAT_SESSION` holds its uid there.
- Otherwise the verb picks the live session whose root holds the working
  directory. If two match, the session on screen wins, then the one active
  last. If none match and one session is live, the verb uses that one.

A session that the verb cannot reach makes it exit non-zero, with the reason
on stderr. When no session matches, stderr lists every live session.

A request that the session refuses prints `{"error": "..."}` on stdout and
exits 0. Check for the `error` key, not for the exit status.

A range, a point, or a key sequence that does not parse fails before it
reaches the session, with exit status 2 and a usage error on stderr.

### sessions

```sh
stoat api sessions
```

```json
[
  {
    "uid": "18dfd12b4c3dbeb0",
    "name": "grand bison",
    "git_root": "/home/me/app",
    "active": true,
    "last_active_ms": 1792381160609,
    "focus": "/home/me/app/src/main.rs",
    "pid": 371545,
    "repo_root": "/home/me/app",
    "tab": 0,
    "tabs": 1,
    "workspaces": 1
  }
]
```

Every live session, the most recently active first.

- `uid` is the value `--session` and `STOAT_SESSION` take.
- `git_root` is the workspace root. `repo_root` is the root of the git
  repository that holds it, or null outside one.
- `active` is true for the session on screen. `last_active_ms` is the time of
  its last activity, in milliseconds since the Unix epoch.
- `focus` is the file in the focused pane, or null.
- `tab` is the index of the active tab, from 0, and `tabs` is the count of
  tabs. `workspaces` is the count of workspaces in the process, and `pid` is
  its process id.

## Lines and columns

Lines start at 1. A column is a byte offset in its line, also from 1, not a
character count. Both ends of a range are inclusive.

| Form | Means |
|---|---|
| `12` | Line 12, whole |
| `12-18` | Lines 12 through 18, whole |
| `12:5-12:19` | From byte 5 of line 12 through byte 19 |
| `12:5` | One point, for `hover` |

The verbs report ranges in the same form, so a range from `selection` goes
back into `goto`, or into a stop of a walkthrough.

`hover` and `diagnostics` are the exception. They pass the language server's
own objects through, and those count `line` and `character` from 0.

## Read what the user sees

### focus

```sh
stoat api focus
stoat api focus --text
```

```json
{
  "dock": null,
  "editor": {
    "cursor": {"col": 1, "line": 31},
    "diff_view": false,
    "dirty": false,
    "line_count": 61,
    "mode": "normal",
    "path": "/home/me/app/src/main.rs",
    "pinned": false,
    "relative_path": "src/main.rs",
    "visible": {"first": 19, "last": 42}
  },
  "modal": null,
  "pane": {"index": 0, "kind": "editor"},
  "tab": {"count": 1, "index": 0, "title": "main.rs"},
  "walkthrough": {
    "annotation": null,
    "slug": "startup",
    "stop": {"count": 3, "id": "s2", "index": 2, "title": "Loading the config"},
    "title": "How startup works"
  },
  "workspace": {
    "active": true,
    "git_root": "/home/me/app",
    "name": "grand bison",
    "uid": "18dfd12b4c3dbeb0"
  }
}
```

What has focus, what the focused editor shows, and where the reader is in an
open walkthrough.

- `pane.kind` is `editor`, `run`, `agent`, `terminal`, `commits`, `image`, or
  `label`. The same words work as the `pane` condition of a key binding.
- `editor` is null when the focused view is no editor. It follows the focus
  into a dock, and `dock` then names the side, `left` or `right`.
- `modal` names the open modal, such as `palette` or `finder`, and is null
  with none. Only the session on screen reports a modal.
- `cursor` is the cell of the block cursor. `visible` holds the first and the
  last line on screen, and is null before the editor's first paint.
- `dirty` is true with unsaved edits. `pinned` and `diff_view` tell whether
  the editor is pinned and whether it shows the diff view.
- `walkthrough` names the open tour by `slug` and `title`. `stop` is the stop
  the reader is on, with its `index` from 1 of `count` and its own `title` or
  null. `annotation` holds the `id`, `label`, `index`, and `count` of the
  annotation the reader is on, or null on the stop's own focus. The field is
  null with no tour open.
- `--text` adds `visible_text`, the text of the lines on screen.

### selection

```sh
stoat api selection
stoat api selection --text
```

```json
{
  "count": 1,
  "mode": "normal",
  "path": "/home/me/app/src/main.rs",
  "relative_path": "src/main.rs",
  "selections": [
    {
      "cursor": {"col": 14, "line": 9},
      "empty": false,
      "end": {"col": 14, "line": 9},
      "primary": true,
      "range": "9:9-9:14",
      "reversed": false,
      "start": {"col": 9, "line": 9},
      "text": "config"
    }
  ]
}
```

Every selection of the focused editor, in the editor's own order.

- `start` and `end` are inclusive. `end` is the first byte of the last
  character selected. `range` gives the same span in the range form.
- `cursor` sits on `end` for a forward selection and on `start` for a
  reversed one. `primary` marks the main selection.
- A selection of zero width sets `empty`, with `start`, `end`, and `cursor`
  at one point. The range form has no empty span.
- `--text` adds `text`, the bytes each selection covers.
- With no editor in focus, the reply is `{"error": "no editor has focus"}`.

### buffers

```sh
stoat api buffers
```

```json
{
  "buffers": [
    {
      "dirty": true,
      "origin": "named",
      "path": "/home/me/app/src/config.rs",
      "preview": false,
      "read_only": false,
      "relative_path": "src/config.rs",
      "shown": [{"dock": "right"}]
    },
    {
      "dirty": false,
      "origin": "visited",
      "path": "/home/me/app/src/main.rs",
      "preview": false,
      "read_only": false,
      "relative_path": "src/main.rs",
      "shown": [{"pane": 0, "tab": 0}]
    }
  ],
  "count": 2
}
```

Every buffer of the workspace. Buffers with a file come first, by path.

- `origin` is `named` for a file a command opened by name, and `visited` for
  a file that a jump or a walk only passed through.
- A scratch buffer has a null `path`. `relative_path` is null for a file
  outside the root too.
- `shown` lists each tab and pane, then each dock, whose editor holds the
  buffer. An empty list means that no pane shows it.
- `preview` marks the scratch buffer behind a picker's preview pane.

### layout

```sh
stoat api layout
```

```json
{
  "active_tab": 0,
  "docks": [],
  "focus": "pane",
  "size": {"cols": 120, "rows": 30},
  "tabs": [
    {
      "active": true,
      "index": 0,
      "panes": [
        {
          "area": {"height": 30, "width": 60, "x": 0, "y": 0},
          "focused": false,
          "index": 0,
          "kind": "editor",
          "path": "/home/me/app/src/main.rs",
          "title": "main.rs",
          "widened": false
        },
        {
          "area": {"height": 30, "width": 60, "x": 60, "y": 0},
          "focused": true,
          "index": 1,
          "kind": "terminal",
          "path": null,
          "title": "term",
          "widened": false
        }
      ],
      "renamed": false,
      "title": "term"
    }
  ]
}
```

The terminal size, every tab with its panes, and every dock.

- `focus` is `pane` or `dock`. `size` and each `area` count terminal cells.
- Only the active tab's panes carry `area`. The panes of another tab have
  none until that tab is on screen again.
- `widened` marks a pane that fills its tab. `renamed` marks a tab with a
  name of its own.
- Each dock holds `side`, `visibility` (`open`, `minimized`, or `hidden`),
  `width` (null unless open), `kind`, `title`, and `focused`.

## Show the user a place

### goto

```sh
stoat api goto src/main.rs 9:9-9:14
```

```json
{"path": "/home/me/app/src/main.rs", "range": "9:9-9:14", "reply": "goto"}
```

Shows a file with a range selected and on screen. The reply gives the
absolute path and the range that the editor selected. A whole-line range comes
back in the byte form, such as `3-5` as `3:1-5:12` when line 5 holds 12 bytes.

Keyboard focus stays where it is. The file shows in the focused pane, unless
that pane shows the agent, a shell, or a run. Then it shows in another pane or
in a fresh split.

A range past the end of the file replies an error, such as
`{"error": "line 40 is past the 11 lines of content"}`.

### open

```sh
stoat api open src/main.rs src/config.rs
stoat api open --split src/main.rs
stoat api open --wait --split REVIEW.md
```

Opens files beside the user and prints nothing.

- Called from a terminal pane of the session, the files cover that pane.
- Elsewhere, `--split` opens them in a fresh split beside the focused pane.
  Without it, they open in the focused pane, unless that pane shows the
  agent, a shell, or a run.
- `--wait` holds until every file opened leaves the editor. Exit status 1
  means that a file left with unsaved edits, as an `$EDITOR` caller expects.

## Drive the editor

### act

```sh
stoat api act MoveDown
stoat api act 'SetMode("insert")'
stoat api act SaveBuffer
stoat api act OpenFileFinder
```

```json
{"action": "SetMode", "reply": "act"}
```

Runs one action, written the way a key binding in the stoat config names it,
with its arguments in parentheses. The action runs as that binding runs it,
and the reply names the action. An action the editor does not know replies
`{"error": "unknown action `NoSuchAction`"}`.

### keys

```sh
stoat api keys 'ihello<Esc>'
stoat api keys '<Space>p'
```

```json
{"count": 7, "reply": "keys"}
```

Types keys as a person types them, and replies the count of keys sent. The
keys use the `--inputs` grammar:

- A bare character is its key, and a space is the space key.
- Named keys go in angle brackets, such as `<Esc>`, `<Enter>`, `<Tab>`, and
  `<Space>`.
- A modifier goes before the key, such as `<C-w>` and `<A-Left>`.
- `<lt>` is a literal `<`.
- `<Wait:N>` sends nothing here.

**Warning:** The keys go to whatever has keyboard focus, and that includes
the agent's own pane. Read `focus` first. When `act` or `goto` does the job,
use it instead.

## Language servers

### hover

```sh
stoat api hover src/main.rs 9:18
```

```json
{
  "contents": {"kind": "markdown", "value": "```rust\nfn load(path: &PathBuf) -> String\n```"},
  "range": {"end": {"character": 21, "line": 8}, "start": {"character": 17, "line": 8}}
}
```

The language server's hover at a point, as its own Hover object. The file
must be open in the session, or the reply is `{"error": "not open"}`. The
reply is null when the server has nothing to show, and a line past the end
replies `{"error": "no line 40"}`.

### diagnostics

```sh
stoat api diagnostics
stoat api diagnostics --path /home/me/app/src/main.rs
```

```json
[
  {
    "diagnostics": [
      {
        "code": "E0425",
        "message": "cannot find function `lod` in this scope",
        "range": {"end": {"character": 20, "line": 8}, "start": {"character": 17, "line": 8}},
        "severity": 1,
        "source": "rustc"
      }
    ],
    "path": "/home/me/app/src/main.rs"
  }
]
```

Without `--path`, every file with diagnostics. With it, the list for that one
file, and `[]` for a file with none.

- `--path` takes an absolute path. A relative path matches no file.
- Each entry is the server's own Diagnostic. `severity` is 1 for an error, 2
  for a warning, 3 for information, and 4 for a hint.

### lsp-status

```sh
stoat api lsp-status
```

```json
{
  "active": true,
  "servers": [
    {
      "capabilities": {"hoverProvider": true, "positionEncoding": "utf-8"},
      "name": "rust-analyzer",
      "pid": 371587
    }
  ],
  "spawn_attempted": true
}
```

The language servers of the session. `active` is true while one runs, and
`spawn_attempted` is true once the session tried to start one. Each server
gives its name, its process id, and the capabilities it announced. A server
that runs inside stoat has a null `pid`.

## Follow the user

### watch

```sh
timeout 300 stoat api watch
```

```json
{"focus": {"dock": null, "editor": {"cursor": {"col": 21, "line": 9}}}, "selection": {"count": 1}}
```

Prints the `focus` and `selection` reports as one line, then one more line
each time either report changes, until the session ends. The reports carry no
text.

Run it in the background, and bound it with `timeout` or with a read of a
fixed count of lines. A reader that stops leaves the command running until
the next change. So `stoat api watch | head -n 1` waits for a change before
it returns. To read the view once, use `focus` and `selection`.

## Recipes

Point the user at the code under discussion, then check that it is on screen:

```sh
stoat api goto src/config.rs 12-20
stoat api focus
```

`focus` names `src/config.rs`, and its `visible` span holds lines 12 to 20.

Explain what the user selected:

```sh
stoat api selection --text
```

Read `relative_path`, `range`, and `text` of the primary selection, then
explain that code.

Hand the user a file to review, and wait until they close it:

```sh
stoat api open --wait --split REVIEW.md
```

The command returns when the file leaves the editor. Exit status 1 means that
the user left unsaved edits.

Check a symbol before an edit:

```sh
stoat api hover src/main.rs 9:18
stoat api diagnostics --path "$PWD/src/main.rs"
```

`hover` needs the file open in the session. Open it with `open` first if
`buffers` does not list it.

## Not for agents

`stoat api hook` is the channel that the hooks of a session's own agent pane
use to report status. An agent does not call it.
