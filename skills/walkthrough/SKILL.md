---
name: walkthrough
description: Author guided code walkthroughs with the `stoat walkthrough` CLI. Use when someone asks to be shown or walked through how something works in a stoat workspace -- "show me how X works", "walk me through the Y path", "give me a tour of Z" -- so the explanation is saved as an ordered set of file ranges with narration instead of vanishing with the conversation.
user_invocable: true
---

# walkthrough

A walkthrough is a saved tour of a codebase. It is an ordered list of **stops**,
each naming one range of one file, a markdown **narration** of what that code
does, and optional labeled **annotations** over smaller ranges in that file or
in another beside it.

Reach for this when a "show me how X works" answer would otherwise be a wall of
chat text. A walkthrough outlives the conversation, travels with the repository,
and can be checked later to see whether the code moved out from under it.

Author it as you explain. Write the stops in reading order, then hand the user
the slug.

## The model

| Thing | What it is |
|---|---|
| Walkthrough | A slug, a title, and an ordered list of stops. One JSON file. |
| Stop | One focus range of one file, plus narration. Id `s1`, `s2`, ... |
| Annotation | A labeled range, in that stop's focus file or in another. Id `a1`, `a2`, ... |
| Focus | Path, range, and the bytes the range covered when captured. |

Ids are stable handles. They are assigned once and never reused, so `s2` keeps
meaning the same stop after you remove `s1`. Use them for every edit, move, and
remove.

An annotation points into its stop's focus file unless `--file` names another
one. Reach for that when a stop is about two files at once, such as a call and
the function it lands in.

## Where files live

`.stoat/walkthroughs/<slug>.json`, under the git repository root. Every command
finds that root by walking up from the current directory, so run them from
inside the workspace. Pass `--workspace <path>` to name the root explicitly.

A slug is lowercase letters, digits, and dashes, starting with a letter or
digit, at most 64 characters. It is the filename, so nothing else is accepted.

Stop paths are stored relative to the root, which is what lets a walkthrough
survive the repository being cloned somewhere else. A `--file` outside the
workspace is refused.

## Ranges

`--range` takes three forms. Lines and columns are **1-based** and both ends are
**inclusive**.

| Form | Means |
|---|---|
| `12` | Line 12, whole |
| `12-18` | Lines 12 through 18, whole |
| `12:5-12:19` | From byte 5 of line 12 through byte 19, inclusive |

A column is a **byte offset within its line**, not a character or a display
column. Mixed forms like `12-18:4` are refused.

Columns are easy to miscount. An out-of-range column is an error, not a silent
clip, so a failed `add-annotation` usually means the count was off by a few.
Prefer the whole-line forms unless the point you are making is genuinely
sub-line, and count from the file rather than from memory.

## Commands

Every command takes `--workspace <path>`. It is omitted below.

### Create and inspect

```sh
stoat walkthrough new startup --title "How startup works"
# .stoat/walkthroughs/startup.json

stoat walkthrough list
# startup	2	How startup works     (slug, stop count, title; tab separated)

stoat walkthrough show startup
# the stored JSON

stoat walkthrough delete startup
# .stoat/walkthroughs/startup.json
```

`new` refuses a slug that already exists rather than overwriting authored stops.

### Stops

```sh
stoat walkthrough add-stop startup \
  --file src/main.rs --range 3 \
  --title "Entry point" \
  --narration "Execution begins here."
# s1
```

`add-stop` appends. Pass `--before <stop-id>` to insert ahead of an existing
stop instead. `--title` and the narration are optional.

For narration longer than one line, pipe it in:

```sh
stoat walkthrough add-stop startup --file src/main.rs --range 3 --narration-file - <<'EOF'
The entry point. Two things happen before anything else:

- the config is read
- the runtime is started
EOF
```

`--narration-file <path>` reads a file, and `-` reads stdin. It is exclusive
with `--narration`.

```sh
stoat walkthrough edit-stop startup s1 --title "Where it starts"
stoat walkthrough edit-stop startup s1 --range 3-6      # re-captures the snippet
stoat walkthrough edit-stop startup s1 --no-title       # drop the title
stoat walkthrough remove-stop startup s1                # takes its annotations too
```

`edit-stop` re-captures the focus snippet only when you pass `--file` or
`--range`. A title-only or narration-only edit leaves the capture alone, so it
never quietly makes a stale stop look current.

```sh
stoat walkthrough move-stop startup s3 --before s1
stoat walkthrough move-stop startup s3 --after s1
stoat walkthrough move-stop startup s3 --last
```

Exactly one of the three is required.

### Annotations

```sh
stoat walkthrough add-annotation startup s1 \
  --range 3:14-3:27 --label "the error type"
# a1

stoat walkthrough add-annotation startup s1 \
  --file src/error.rs --range 8:1-8:24 --label "where it is raised"
# a2

stoat walkthrough edit-annotation startup s1 a1 --label "the error alias"
stoat walkthrough edit-annotation startup s1 a1 --range 3:14-3:20   # re-captures
stoat walkthrough edit-annotation startup s1 a2 --no-file --range 4  # back to the focus
stoat walkthrough remove-annotation startup s1 a1
```

The range is within `--file`, or within stop `s1`'s focus file when you omit it.
As with `edit-stop`, only a `--file`, `--no-file`, or `--range` re-captures the
snippet.

A `--range` on its own re-captures from whichever file the annotation already
reads against, so a cross-file annotation stays where it is. Pair `--file` with
a `--range` when you move one, since the stored range is exact bytes and rarely
lands well in a different file.

### Check

```sh
stoat walkthrough check           # every walkthrough
stoat walkthrough check startup   # just this one
```

Prints nothing and exits 0 when every range still reads what it captured.
Otherwise it prints one line per finding and exits 1:

```
startup/s1: stale: captured "fn main() {", found "fn MAIN() {"
startup/s1/a1: error: cannot read src/main.rs
```

- `stale`: the range still resolves but covers different bytes. Usually the
  code shifted. Re-point it with `edit-stop --range`, or re-capture it.
- `error`: the file is gone, or the range no longer fits the file. The stop
  needs a new `--file` or `--range`.

Gate on the exit status, not on parsing the output.

## Authoring guidance

- **3 to 8 stops** for a typical explanation. Fewer and it is a comment. More
  and it is a file listing.
- **Stops go in reading order**, the order you would walk someone through,
  rather than the order the code appears in the file.
- **Narration carries the why.** The reader can see the code. Tell them what it
  is for and what to notice.
- **Annotate sparingly.** Add one only where a short label says something the
  narration does not, such as naming a subexpression or pointing at the arm
  that matters. If the narration already says it, skip the annotation.
- **Run `check` last, always.** Repair anything it reports before you hand the
  user the slug. A walkthrough that was stale the moment it was written is
  worse than none.
- **Edit by id, never by hand.** The commands own id assignment and snippet
  capture. A hand-written snippet is a lie the moment it is wrong, and `check`
  compares against exactly that field.

## What a walkthrough looks like

Two stops over `src/main.rs`, with one annotation. This is what `show` prints,
abridged after the first stop:

```json
{
  "slug": "startup",
  "title": "How startup works",
  "git_head": null,
  "next_stop_id": 3,
  "next_annotation_id": 2,
  "stops": [
    {
      "id": "s1",
      "title": "Entry point",
      "narration": "Execution begins here.",
      "focus": {
        "path": "src/main.rs",
        "range": {
          "start": { "line": 3, "col": 1 },
          "end": { "line": 3, "col": 29 }
        },
        "snippet": "fn main() -> io::Result<()> {"
      },
      "annotations": [
        {
          "id": "a1",
          "range": {
            "start": { "line": 3, "col": 14 },
            "end": { "line": 3, "col": 27 }
          },
          "snippet": "io::Result<()>",
          "label": "the error type"
        }
      ]
    }
  ]
}
```

Every range carries the bytes it covered when captured. That is what `check`
compares against, and it is why the commands capture snippets for you rather
than trusting anything typed by hand.
