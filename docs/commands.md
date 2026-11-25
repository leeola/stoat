# Commands

A command runs shell lines, editor actions, or a tool from the command
palette. You declare commands in the editor config, `config.stcfg`, next to
key bindings and themes. Each command has a name, typed arguments, a run,
and an output.

## Where commands show

Open the command palette with `:`. Its root list has a row for each command.
The row shows the name of the command, its arguments, its doc text, and a
Source Tag at the right end. The Source Tag shows the kind of the command:

- `config` marks a command block, `command NAME { ... }`.
- `script` marks a command block whose `run` is a list of steps.
- `cli` marks a wrapped tool, `command NAME = cli(...);`.

Push `S-Tab` to change the scope of the list. The `config` scope shows only
command blocks. The `cli` scope shows only wrapped tools. A scope is
available only when the config declares a command of its kind.

The doc island of the palette shows the selected command. Its last line
shows the label of the Source Tag, the word `command`, a middle dot, and the
config line that declares the command, for example `config.stcfg:12`.

To run a command, type its name, a space, and its arguments. Then push
Enter.

## The two forms

A command block declares a command with statements:

```text
command NAME { STATEMENT; STATEMENT; }
```

A wrapped tool declares a command that runs a tool with the text that you
type after its name:

```text
command NAME = cli("TOOL", NAME: VALUE);
```

A named argument has a colon between its name and its value, for example
`output: locations` or `show: "#{number} {title}"`.

## Statements

A command block can hold these statements. Each statement ends with a
semicolon.

- `doc = "TEXT";` gives the description that the palette shows.
- `arg NAME = KIND;` declares an argument. The arguments have the order of
  their `arg` statements.
- `run = STEP;` or `run = [STEP, STEP];` gives what the command runs. Each
  command block must have a `run` statement.
- `output = NAME;` gives where the output goes. The default is `block`.

A command block sets `doc`, `run`, and `output` one time each at most.

This command block checks out a pull request that you pick from a list:

```stcfg
command pr {
    doc = "check out a pull request";
    arg pr = pick(
        "gh pr list --json number,title,headRefName,state",
        show: "#{number} {title}",
        value: "{number}",
        preview: "gh pr view {number}",
    );
    run = shell("gh pr checkout {pr}");
    output = notify;
}
```

## Arguments

An argument has one of these kinds:

- `text` takes the text that you type.
- `text(default: VALUE)` takes a default when you leave the argument empty.
  The default is a string, a number, `true`, `false`, or a context value.
  See [Context values](#context-values).
- `file` takes a file of the workspace. The palette completes it as `:o`
  does.
- `dir` takes a directory of the workspace. The palette completes it as
  `:cd` does.
- `pick(...)` takes a row of a list that a shell line prints. See
  [Pick lists](#pick-lists).
- `same_as(NAME)` takes the kind of the first argument of the command
  `NAME`.

An argument with a default is optional, and the palette shows it as
`[name]`. Each other argument is necessary, and the palette shows it as
`<name>`.

When a command has one argument, that argument takes all the text after the
name. When a command has two or more arguments, each argument takes one
word, and the last argument takes the rest of the text. Thus a message at
the end keeps its spaces.

A `file` or `dir` value that starts with `~/` runs with your home directory
in place of the `~`.

### Pick lists

`pick("LIST", show: "TEMPLATE", value: "TEMPLATE", preview: "LINE")` runs
the shell line `LIST` in the workspace root when the argument entry starts.
`LIST` cannot hold a placeholder, because no argument has a value at that
time.

The palette makes a row of the list from each row of the output:

- A JSON array gives one row for each element. Each key of an object gives
  a field of the same name. A string field is the string, `null` is empty
  text, and each other value is its JSON text. Each row also has the field
  `line`, which holds the JSON text of the element, or the string of a
  string element.
- Other output gives one row for each line that is not blank. The field
  `line` holds the line.

The list shows at most 5000 rows. The named arguments of `pick` are
optional:

- `show` gives the text of a row, which the typed text matches. The
  default is `{line}`.
- `value` gives the text that Enter and Tab write for the row. The default
  is the `show` template.
- `preview` gives a shell line that prints a preview of the selected row.

`Ctrl-r` runs the list again. The key line of the palette shows the shell
line that Enter runs.

This command picks a pull request from the same list as `pr`. Then it opens
the per-file diff against `origin/main`:

```stcfg
command prdiff {
    doc = "open a PR as diff-changes against its base";
    arg pr = same_as(pr);
    run = [shell("gh pr checkout {pr}"), DiffFiles(rev: "origin/main")];
}
```

## Context values

A default can read a value from the editor when the command runs:

- `git.last_subject` is the summary of the newest commit of the repository
  of the workspace.
- `selection` is the text of the primary selection.
- `file` is the path of the file of the pane. A file under the workspace
  root has a path relative to the root.
- `line` is the line number of the primary cursor. The first line is 1.

The selection, the file, and the line come from the editor of the focused
pane, under the palette. A context value with nothing to read is empty.

This script tests, pushes, and opens a pull request that has the summary of
the newest commit as its title:

```stcfg
command ship {
    arg message = text(default: git.last_subject);
    run = [
        shell("cargo test"),
        shell("git push -u origin HEAD"),
        shell("gh pr create --fill --title {message}"),
        Notify("shipped"),
    ];
    output = run_pane;
}
```

## Steps

A step is one of these:

- `shell("LINE")` runs a shell line in the workspace root.
- An action call runs an editor action as a key binding does, for example
  `Notify("shipped")` or `DiffFiles(rev: "origin/main")`. The call takes
  only the parameters that the action declares. A named argument must stand
  at the place of its parameter. An action argument cannot be a context
  value.
- The bare name of a command block is a command step. It runs the steps of
  that command in its place.

A `run` statement names one step, or a list of steps in square brackets. A
command whose `run` is a list is a script, also when the list holds one
step. The palette shows a script with the `script` Source Tag.

The steps run in order. Each step runs after the step before it ends with
the exit code 0. The first step that fails stops the run, and the output
shows the failure.

A command step runs the steps of the command that it names. Each argument
of that command takes the value of the argument of the same name of the
caller. When the caller does not declare that argument, the argument takes
its default. Thus each argument that the caller does not declare must have
a default. A command step cannot name a wrapped tool, and a command cannot
run itself through its steps.

## Placeholders

A string of a step can hold placeholders:

- `{name}` writes the value of the argument `name`. Each placeholder must
  name an argument of the command.
- `{{name}}` writes the text `{name}`.
- `${` stays as it is, so a shell variable such as `${HOME}` stays.

Each other brace is plain text, so `awk '{print $1}'` needs no change.

In a shell line, a value outside quotes becomes one shell word, so a title
with spaces stays one argument. A value that holds only ASCII letters,
digits, and the characters `_@%+=:,./-` stays bare. Each other value goes
in single quotes, and an empty value is `''`. A value inside double quotes
gets a backslash before each backslash, double quote, dollar sign, and
backquote. A value inside single quotes writes each single quote as
`'\''`. In an action argument, the value stays as it is.

The `show`, `value`, and `preview` templates of a `pick` read the fields of
a row: `line`, and each key of a JSON object.

## Outputs

`output = NAME;` names one of five outputs:

- `block` shows the run in a modal. The modal shows the output of the shell
  line. At the end it shows `done`, or `exited` and the exit code. This is
  the default.
- `notify` shows a status message. When the run succeeds, the message is
  the first line of the output that holds text, or `NAME done`.
- `buffer` opens the output in a new scratch buffer. A run that fails opens
  no buffer.
- `run_pane` runs the shell line in a Run pane. The command uses the Run
  pane on show, or it opens a new one. The Run pane shows the output.
- `locations` opens a location list over the lines of the output that name
  a location, as `path:line:col: text` does. The column and the text are
  optional. The list opens whatever the exit code, because a search tool
  exits with 1 when it finds nothing. With no location, the message is
  `no locations`.

When a step of a `notify`, `buffer`, or `locations` run fails, a status
message shows the name of the command, the first line of the error output,
and the exit code, for example `ship: rejected (exit 1)`. The last step of a
`locations` run shows this failure only when it found no location.

A shell step that does not run in the modal or in a Run pane runs without a
terminal, so its output and its error output stay apart. `buffer` and
`locations` read the output of each shell step of the run, in order.
`notify` reads the output of the last shell step. A run that ends with an
action step shows no message of its own.

Under `block` or `run_pane`, a list of shell steps alone runs as one shell
line, with `&&` between the steps. A list with an action step runs step by
step under these outputs, and it shows its result as a `notify` run does.

An action step runs only while the workspace that was active at the start
of the run stays active. When you change the workspace, the run stops with
`NAME: stopped, the workspace changed`.

## Wrapped tools

`command NAME = cli("TOOL", ...);` wraps a tool. `TOOL` is one or more
shell words. A wrapped tool has no arguments of its own. The text that you
type after its name follows the tool on the shell line as you typed it.
Thus quotes, flags, and globs keep their shell meaning.

The call takes these named arguments, which are optional:

- `output: NAME` gives the output. The default is `block`.
- `doc: "TEXT"` gives the description that the palette shows.
- `completions: "LINE"` gives a shell line that prints the fish completion
  script of the tool.

When the argument entry of the command opens for the first time, the
palette runs `TOOL --help` and then `TOOL --version`. With `completions`,
the palette runs that line in place of `TOOL --help`. The palette reads the
usage line, the flags, and the subcommands from the help, and it completes
them as you type. The doc island shows where the help came from, and the
version of the tool. When the shell does not find the tool, the doc island
shows `TOOL: not found`, and the command still runs your text as you typed
it.

```stcfg
command rg = cli("rg", output: locations);
command jj = cli("jj", completions: "jj util completion fish");
```

## Names

A command name cannot be a name that a built-in command, an action, or a
command group of the palette answers to. Such a name is an error. A name is
unique, and the case of ASCII letters is not important. When two items
declare one name, the later item is an error.

A command of your config replaces a shipped command of the same name.

## Errors

A `command` item with an error does not load. The other items of the config
load.

When your config has an error, the editor shows a status message with the
first error, for example ``config.stcfg:3: unknown output `nofify` ``. With
more errors, the message ends with the count of the others, for example
`(+2 more)`.

When you open `config.stcfg` in the editor, each error shows as a
diagnostic at its place.
