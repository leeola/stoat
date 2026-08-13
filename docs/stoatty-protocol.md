# The stoatty protocol

Stoatty is a terminal emulator that renders more than cells. A program running
inside it can ask for a rounded panel, a run of text at a fractional scale, a
minimap strip, or a smooth-scrolling page pool, all drawn by the GPU off the
character grid.

Those requests travel as APC escape sequences, so the same program still runs in
any other terminal. This document is what a program outside this repository
needs to speak the protocol.

The Rust crate is `stoatty_protocol` (`tty_protocol/`). It is dependency-light
on purpose: no GPU, windowing, or terminal-state crates, so linking it costs a
program almost nothing.

## The frame

Every request is one APC string:

```
ESC _ Gstoatty ; <sub-command> ; <base64-arg> ; <base64-arg> ... ESC \
```

- `ESC _` opens an Application Program Command string. Conformant terminals
  consume it and draw nothing, which is what makes the protocol safe to emit
  anywhere.
- `Gstoatty` claims the namespace. A frame with any other prefix is ignored.
- `<sub-command>` is a bare ASCII name, listed in the table below.
- Each argument is standard base64, so binary payloads survive a text stream.
  Arguments are positional and separated by `;`.
- `ESC \` (String Terminator) ends the frame. A bare `BEL` (`0x07`) is accepted
  as an alternate terminator, since intermediaries emit it.

Two limits apply, both in `frame.rs`:

| Limit | Value | Behavior past it |
|---|---|---|
| `MAX_APC_PAYLOAD` | 64 KiB | The scanner drops the frame **whole**, not truncated. |
| `MAX_FRAME_ARGS` | 8 | The frame is rejected whole rather than read as a prefix. |

The payload budget covers everything between `ESC _` and the terminator, so the
`Gstoatty;<sub>;` prefix and the base64 expansion both count against it. Base64
turns 3 bytes into 4, so the usable raw payload is roughly three quarters of the
cap. An encoder whose argument could grow past it either paginates
(`minimap_lines` does) or refuses in debug (`line_layout` and `polyline` do).

## What degrades, and what does not

**Frames degrade to nothing.** Any terminal that is not stoatty consumes the APC
string and draws nothing. Every command in the table below is safe to emit
blind.

**Streamed content does not degrade.** Three commands carry their payload
*outside* the frame wrapper, as ordinary bytes between an open marker and a
close marker:

| Open | Close | Carries |
|---|---|---|
| `popover` | `popover_end` | The popover's text |
| `text_run` | `text_run_end` | The run's characters |
| `fill` | `fill_end` | A page of VT cells |

That is what frees them from the 64 KiB cap, and it is also what makes them
dangerous: a terminal that never opened the capture prints those bytes over
whatever is on screen. **Settle which terminal answered before emitting any of
them.**

Captured text for `popover` and `text_run` must be plain text. The terminal cuts
the capture at the first `ESC`, so set colors through the head fields rather
than with escape sequences in the text. `fill` content is full VT by design,
since it paints a page.

## Finding out which terminal answered

`stoatty_protocol::detect` gives two answers, and they are not equally good.

**`handshake(&hello, HANDSHAKE_FALLBACK)`** asks the terminal and believes the
reply. It sends a `hello` frame and a cursor-position query, then reads stdin
until one of them answers: an `ident` reply means stoatty, a cursor report means
someone else. It is definitive, and right even where the environment lies.

It costs one round trip and sole ownership of stdin while it runs, so call it
before anything else reads input. It returns the bytes that arrived while it
held stdin, so a program that reads keys replays them instead of losing what
someone typed at launch. `HANDSHAKE_FALLBACK` is two seconds, and it is a
backstop rather than a latency budget: on a real terminal one of the two replies
ends the wait, so link latency never decides the verdict.

**`env_says_stoatty()`** reads `STOATTY=1`, which stoatty sets in every process
it spawns. It is free and needs no stdin, and it is wrong the moment another
terminal runs nested inside a stoatty session, because the variable is
inherited. Treat it as a hint.

**Precedence: a handshake result always overrides the environment.** Reach for
the environment only where the handshake cannot run at all -- before stdin can
be owned, in a child process with no way to probe, or on a fast path where being
wrong only costs a plainer frame.

Stoatty also exports `TERM_PROGRAM=stoatty` and `TERM_PROGRAM_VERSION`, the
convention the wider ecosystem reads. Those name the terminal; they do not say
what it can render. Gate a feature on the protocol version in the `ident` reply.

## Lanes: what a reset clears

`reset` clears the terminal's accumulated **decorations**. Everything else
survives it. This split decides how an emitter batches its frames, and it is the
thing most likely to surprise.

A **decoration** is re-declared every frame. Dropping it from the scene is how
it leaves the screen, so an emitter re-sends the whole set behind a `reset`.

A **persistent** command updates state the terminal keeps. A `reset` in front of
it is wasteful or destructive: a re-declared `scroll_region` whose region was
just cleared restarts its eased scroll from nothing rather than gliding.

## Command table

Lane is `D` for decoration (cleared by `reset`), `P` for persistent, `C` for a
session control. "Head" is the fixed prefix of the first argument.

| Sub-command | Lane | Head | Notes |
|---|---|---|---|
| `border` | D | 12 B | Rectangle, style, color |
| `panel` | D | 19 B+ | Rectangle, fill, border, radius, shadow |
| `scale` | D | 5 B | Draw the cell's glyph at N times cell size |
| `popover` | D | 23 B | + streamed content, closed by `popover_end` |
| `popover_end` | D | -- | Commits the capture |
| `icon` | D | 9 B, 13 B with offset | Renderer-drawn status sigil |
| `text_run` | D | 12 B legacy, 13 B with bg flag | + streamed text, closed by `text_run_end` |
| `text_run_end` | D | -- | Commits the capture |
| `bar` | D | 11 B | Thin rect in sixteenths of a cell |
| `polyline` | D | 5 B + 4 B per point | Max 12283 points |
| `line_layout` | D | 2 B per line | Max 24567 lines, replaced whole |
| `minimap` | D | 29 B + palette arg | Strip declaration; its content store is persistent |
| `scroll_region` | D | 10 B | Eased by the change between declarations |
| `fill` | P | 12 B | + streamed page, closed by `fill_end` |
| `fill_end` | P | -- | Commits the page onto its pool slot |
| `pool_region` | P | 16 B | Declares a smooth-scroll pool |
| `scroll` | P | 14 B | Pool scroll target, page plus fraction |
| `pool_cursor` | P | 14 B | Anchors the cursor to a gliding pool |
| `reposition` | P | 12 B | Re-anchors across an unbuffered gap |
| `pool_drop` | P | 4 B | Retires a pool |
| `minimap_lines` | P | 16 B header + lines | Paginates past the payload cap |
| `minimap_view` | P | 10 B | Moves the viewport thumb |
| `minimap_drop` | P | 4 B | Retires a content store |
| `window_open` | P | 8 B + title | Aux OS window as a second render target |
| `window_close` | P | 4 B | |
| `window_focus` | P | 4 B | |
| `reset` | C | -- | Clears every decoration |
| `config_reload` | C | -- | The terminal re-reads its own config file |
| `zoom_capture` | C | 1 B | Claim or release the platform zoom combo |
| `font_step` | C | 4 B | Step the terminal's font size |
| `hello` | C | 5 args | Identifies the program; the terminal replies `ident` |

`ident` travels the other way, terminal to program, and arrives as input bytes
on stdin. `decode` never yields it; use `decode_ident_reply`.

**The decoders are normative for field order and offsets.** The `decode_*`
functions in `tty_protocol/src/command.rs` are what the terminal actually runs,
and each command's payload layout is documented on its type there. Read those
rather than inferring layout from the head sizes above, which are given so an
implementer can size a buffer and recognize a truncated frame.

## Reading a stream back

`command::decode` parses one frame. `command::decode_stream` walks a whole
emitted byte stream and returns the commands in order, stitching each
`popover` and `text_run`'s streamed content back onto its command. That is what
an emitter asserts its own output with, and it steps over a filled page's escape
sequences rather than mistaking them for frames.

## Evolving a command

A command grows only by appending, never by reordering, resizing, or
repurposing what is already there. The reason and the full rule live next to
`PROTOCOL_VERSION` in `tty_protocol/src/lib.rs` -- read it there before adding a
field, since the two sides of a session are versioned separately and an older
terminal has to keep understanding the prefix it knows.

## Where to look next

| For | Read |
|---|---|
| The frame grammar in code | `tty_protocol/src/frame.rs` |
| Every command and its payload | `tty_protocol/src/command.rs` |
| Detection | `tty_protocol/src/detect.rs` |
| Ratatui widgets that emit these frames | `widgets/` |
| Runnable examples | `cargo run --example panel`, and its siblings in `tty/examples/` |
| Why non-cell components exist at all | `docs/stoatty-non-cell-components.md` |
