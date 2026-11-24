//! The usage, the flags, and the subcommands of a command line tool, as its
//! help text describes them.
//!
//! A `cli` user command wraps a tool whole, so the palette reads the tool's
//! own help to name its usage and to offer its flags and subcommands. The
//! parser reads the help shapes of clap, GNU, and ripgrep. It answers what it
//! finds and has no error case, because a help text in no shape it reads still
//! belongs to a tool that the command runs.

use std::{
    iter::{self, Peekable},
    mem,
    str::Chars,
};

/// The text that opens a usage line, ASCII case ignored.
const USAGE: &str = "usage:";

/// The value name of a flag that a fish completion script marks as taking a
/// value, because the script names no value.
const FISH_VALUE: &str = "VALUE";

/// What a tool's help text describes.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CliSpec {
    /// The first usage line as a palette signature, such as
    /// `[flags] <pattern> [path...]`, or `None` for a help with no usage line.
    pub(crate) usage: Option<String>,
    pub(crate) flags: Vec<CliFlag>,
    pub(crate) subcommands: Vec<CliSubcommand>,
}

/// A flag of a tool, as one line of its help's flag list declares it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CliFlag {
    short: Option<char>,
    long: Option<String>,
    /// The name of the value that the flag takes, such as `GLOB`, or `None`
    /// for a switch.
    pub(crate) value: Option<String>,
    /// The first sentence of the flag's description, with no closing period.
    pub(crate) description: String,
}

/// A subcommand of a tool, or an alias of one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CliSubcommand {
    pub(crate) name: String,
    /// The text of the subcommand's row, or `alias for <name>` for an alias.
    pub(crate) description: String,
}

/// The options of one fish `complete` statement that the parser reads, each
/// the first value that the statement gives it.
#[derive(Default)]
struct Complete<'a> {
    /// The command that the statement completes, from `-c`.
    command: Option<&'a str>,
    /// The condition under which the statement applies, from `-n`.
    condition: Option<&'a str>,
    short: Option<&'a str>,
    long: Option<&'a str>,
    description: Option<&'a str>,
    /// The candidates that the statement offers, from `-a`.
    arguments: Option<&'a str>,
    /// Whether `-r` or `-x` marks the option as taking a value.
    takes_value: bool,
}

/// The usage, the flags, and the subcommands that `text`, a tool's help,
/// describes.
///
/// A part that the text holds in no shape the parser reads is empty.
pub(crate) fn parse_help(text: &str) -> CliSpec {
    let lines: Vec<&str> = text.lines().collect();
    CliSpec {
        usage: usage_text(&lines).map(usage_signature),
        flags: flags(&lines),
        subcommands: subcommands(&lines),
    }
}

/// The flags and the subcommands that `script`, a fish completion script,
/// declares for the tool `command`.
///
/// The script's `complete` statements whose `-c` names `command` declare
/// them, in the shape that clap writes. A statement with `-s` or `-l`
/// declares a flag, which takes a value when `-r` or `-x` marks it. A
/// statement with neither declares a subcommand when its `-a` is one plain
/// name. A statement whose condition names a subcommand completes that
/// subcommand and is skipped.
///
/// The usage reads `[flags] <command>` for a tool with subcommands and
/// `[flags] [args...]` for one with none. An unclosed quote ends the script,
/// and the statements before it stand.
pub(crate) fn parse_fish_completions(script: &str, command: &str) -> CliSpec {
    let mut flags = Vec::new();
    let mut subcommands = Vec::new();
    for words in fish_statements(script) {
        let Some(complete) = read_complete(&words) else {
            continue;
        };
        if complete.command != Some(command) || complete.condition.is_some_and(names_subcommand) {
            continue;
        }

        let description = complete.description.unwrap_or_default();
        if complete.short.is_some() || complete.long.is_some() {
            flags.push(CliFlag {
                short: complete.short.and_then(|short| short.chars().next()),
                long: complete.long.map(str::to_owned),
                value: complete.takes_value.then(|| FISH_VALUE.to_owned()),
                description: first_sentence(description),
            });
        } else if let Some(name) = complete.arguments.filter(|name| is_subcommand_name(name)) {
            subcommands.push(CliSubcommand {
                name: name.to_owned(),
                description: description.to_owned(),
            });
        }
    }

    let usage = match subcommands.is_empty() {
        true => "[flags] [args...]",
        false => "[flags] <command>",
    };
    CliSpec {
        usage: Some(usage.to_owned()),
        flags,
        subcommands,
    }
}

/// The palette signature of `usage`, a usage line of a tool's help.
///
/// The first word names the program and drops. One `[flags]` leads the
/// signature when the usage holds an options token or a dash token, wherever
/// it stands. A positional in brackets reads `[name]` and any other reads
/// `<name>`, in lower case, with its `...` kept.
fn usage_signature(usage: &str) -> String {
    let mut flags = false;
    let mut parts = Vec::new();
    for token in usage_tokens(usage).into_iter().skip(1) {
        if token == "..." {
            continue;
        }
        if is_flag_token(token) {
            flags = true;
            continue;
        }

        let name: String = token
            .replace("...", "")
            .chars()
            .filter(|c| !matches!(c, '[' | ']' | '<' | '>' | ' '))
            .collect::<String>()
            .to_lowercase();
        if name.is_empty() {
            continue;
        }
        let repeats = token.contains("...");
        parts.push(match (token.starts_with('['), repeats) {
            (true, true) => format!("[{name}...]"),
            (true, false) => format!("[{name}]"),
            (false, true) => format!("<{name}>..."),
            (false, false) => format!("<{name}>"),
        });
    }

    if flags {
        parts.insert(0, "[flags]".to_owned());
    }
    parts.join(" ")
}

/// The text of the first usage line of `lines`.
///
/// The line's text starts with `usage:` in any case. Its usage is the rest of
/// the line, or the next line that holds more than whitespace when the rest is
/// empty, as clap's older help and ripgrep set it.
fn usage_text<'a>(lines: &[&'a str]) -> Option<&'a str> {
    let at = lines.iter().position(|line| {
        text_of(line)
            .get(..USAGE.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(USAGE))
    })?;
    let rest = text_of(lines[at])[USAGE.len()..].trim();
    if !rest.is_empty() {
        return Some(rest);
    }
    lines[at + 1..]
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
}

/// `usage` split at each space that no `[..]` or `<..>` holds.
fn usage_tokens(usage: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (at, c) in usage.char_indices() {
        match c {
            '[' | '<' => depth += 1,
            ']' | '>' => depth = depth.saturating_sub(1),
            ' ' if depth == 0 => {
                if at > start {
                    tokens.push(&usage[start..at]);
                }
                start = at + 1;
            },
            _ => {},
        }
    }
    if start < usage.len() {
        tokens.push(&usage[start..]);
    }
    tokens
}

/// Whether the usage token `token` stands for the tool's flags.
///
/// A dash after a leading `[` or `<` opens a flag, and a token whose letters
/// read `option`, `options`, `flag`, or `flags` names them as a group.
fn is_flag_token(token: &str) -> bool {
    let inner = token.strip_prefix(['[', '<']).unwrap_or(token);
    let letters: String = token
        .chars()
        .filter(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase();
    inner.starts_with('-') || matches!(letters.as_str(), "option" | "options" | "flag" | "flags")
}

/// The flags that the flag lines of `lines` declare, in their order.
///
/// A flag line reads as a flag at the lowest indent of the lines that read as
/// one. A long-only flag four columns deeper is a flag line too, unless a
/// line at that lowest indent starts with `--`. Clap and GNU set a long-only
/// flag under the long column of the others, while ripgrep sets each flag at
/// the lowest indent and its description four columns deeper, where a
/// description line that starts with `--` is no flag.
fn flags(lines: &[&str]) -> Vec<CliFlag> {
    let candidates: Vec<usize> = (0..lines.len())
        .filter(|&at| is_flag_candidate(lines[at]))
        .collect();
    let Some(base) = candidates.iter().map(|&at| indent(lines[at])).min() else {
        return Vec::new();
    };

    let long_at_base = candidates
        .iter()
        .any(|&at| indent(lines[at]) == base && text_of(lines[at]).starts_with("--"));
    let flag_lines: Vec<usize> = candidates
        .into_iter()
        .filter(|&at| {
            let depth = indent(lines[at]);
            depth == base
                || (depth == base + 4 && !long_at_base && text_of(lines[at]).starts_with("--"))
        })
        .collect();
    flag_lines
        .iter()
        .map(|&at| flag(lines, at, &flag_lines))
        .collect()
}

/// Whether `line` reads as a flag line.
///
/// Its text starts with `--` and a letter or a digit, or with `-`, one
/// character that is not `-` or a space, and then the end, a space, or a
/// comma.
fn is_flag_candidate(line: &str) -> bool {
    let text = text_of(line);
    if let Some(rest) = text.strip_prefix("--") {
        return rest.starts_with(|c: char| c.is_ascii_alphanumeric());
    }
    let Some(rest) = text.strip_prefix('-') else {
        return false;
    };
    let mut chars = rest.chars();
    match chars.next() {
        Some(first) if first != '-' && first != ' ' => {
            matches!(chars.next(), None | Some(' ' | ','))
        },
        _ => false,
    }
}

/// The flag that line `at` of `lines` declares, one of `flag_lines`.
///
/// The line's text up to its first run of two spaces names the flag's forms,
/// and the text after the run starts its description. The description goes on
/// through the lines below that stand deeper than the flag line, up to an
/// empty line or the next flag line, and keeps its first sentence.
fn flag(lines: &[&str], at: usize, flag_lines: &[usize]) -> CliFlag {
    let (head, first) = split_columns(text_of(lines[at]));
    let mut short = None;
    let mut long = None;
    let mut short_value = None;
    let mut long_value = None;
    for form in head.split(", ") {
        if let Some(rest) = form.strip_prefix("--") {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(rest.len());
            if end > 0 && long.is_none() {
                long = Some(rest[..end].to_owned());
                long_value = value_name(&rest[end..]);
            }
        } else if let Some(rest) = form.strip_prefix('-') {
            let mut chars = rest.chars();
            if let Some(name) = chars.next()
                && short.is_none()
            {
                short = Some(name);
                short_value = value_name(chars.as_str());
            }
        }
    }

    let depth = indent(lines[at]);
    let below = lines
        .iter()
        .enumerate()
        .skip(at + 1)
        .take_while(|&(next, line)| {
            !line.trim().is_empty()
                && flag_lines.binary_search(&next).is_err()
                && indent(line) > depth
        })
        .map(|(_, line)| line.trim());
    let parts: Vec<&str> = iter::once(first)
        .chain(below)
        .filter(|part| !part.is_empty())
        .collect();
    CliFlag {
        short,
        long,
        value: long_value.or(short_value),
        description: first_sentence(&parts.join(" ")),
    }
}

/// The name of the value that `rest`, the text after a flag form's name,
/// declares, or `None` for a form that takes no value.
///
/// The text drops a leading `...`, its leading spaces, a leading `=` or `[`,
/// a trailing `...` and `]`, and one pair of angle brackets, so `=PATTERN`,
/// ` <CODE>`, and `[=WHEN]` each name their value.
fn value_name(rest: &str) -> Option<String> {
    let rest = rest.strip_prefix("...").unwrap_or(rest).trim_start();
    let rest = rest.trim_start_matches(['=', '[']);
    let rest = rest.strip_suffix("...").unwrap_or(rest);
    let rest = rest.strip_suffix(']').unwrap_or(rest);
    let rest = rest
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .unwrap_or(rest);
    (!rest.is_empty()).then(|| rest.to_owned())
}

/// The subcommands that the commands sections of `lines` list.
///
/// A section opens at a heading with no indent, as [`is_commands_heading`]
/// reads one, and its rows are the lines after it at the indent of its first
/// row, up to an empty line or a line with no indent. Each alias of a row
/// follows its name, with the description `alias for <name>`.
fn subcommands(lines: &[&str]) -> Vec<CliSubcommand> {
    let mut found = Vec::new();
    for (at, heading) in lines.iter().enumerate() {
        if !is_commands_heading(heading) {
            continue;
        }

        let section = lines[at + 1..]
            .iter()
            .take_while(|line| !line.trim().is_empty() && indent(line) > 0);
        let mut row_indent = None;
        for row in section {
            let depth = indent(row);
            if *row_indent.get_or_insert(depth) != depth {
                continue;
            }
            let (names, description) = split_columns(text_of(row));
            let mut names = names.split(", ");
            let Some(first) = names.next().filter(|name| is_subcommand_name(name)) else {
                continue;
            };
            found.push(CliSubcommand {
                name: first.to_owned(),
                description: description.to_owned(),
            });
            found.extend(
                names
                    .filter(|alias| is_subcommand_name(alias))
                    .map(|alias| CliSubcommand {
                        name: alias.to_owned(),
                        description: format!("alias for {first}"),
                    }),
            );
        }
    }
    found
}

/// Whether `line` heads a list of subcommands.
///
/// The line has no indent, and its text in lower case is `commands:`,
/// `subcommands:`, `commands`, or `subcommands`, or ends with ` commands:`.
fn is_commands_heading(line: &str) -> bool {
    if indent(line) > 0 {
        return false;
    }
    let text = line.trim_end().to_ascii_lowercase();
    matches!(
        text.as_str(),
        "commands:" | "subcommands:" | "commands" | "subcommands"
    ) || text.ends_with(" commands:")
}

/// Whether `name` reads as the name of a subcommand.
///
/// A name starts with a letter or a digit and goes on in letters, digits,
/// and `_`, `:`, `.`, `+`, or `-`.
fn is_subcommand_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '+' | '-'))
}

/// The statements of `script`, each as its words, with fish's quoting read.
///
/// A statement ends at a newline or a `;` outside quotes, and a `#` that
/// starts a word opens a comment to the end of its line. Outside quotes a
/// backslash takes the next character. Adjacent pieces make one word, so
/// `a'b'"c"` reads `abc`.
///
/// A quote that never closes ends the script, and drops the statement that it
/// opens in, because a quoted value can hold newlines and the rest of the
/// script reads as one value.
fn fish_statements(script: &str) -> Vec<Vec<String>> {
    let mut statements = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | ';' => {
                words.extend(word.take());
                if !words.is_empty() {
                    statements.push(mem::take(&mut words));
                }
            },
            ' ' | '\t' | '\r' => words.extend(word.take()),
            '#' if word.is_none() => while chars.next_if(|&next| next != '\n').is_some() {},
            '\\' => {
                if let Some(next) = chars.next() {
                    word.get_or_insert_with(String::new).push(next);
                }
            },
            '\'' | '"' => {
                if !read_quoted(&mut chars, c, word.get_or_insert_with(String::new)) {
                    return statements;
                }
            },
            c => word.get_or_insert_with(String::new).push(c),
        }
    }

    words.extend(word);
    if !words.is_empty() {
        statements.push(words);
    }
    statements
}

/// Read the piece that the quote `quote` opened into `word`, up to its
/// closing quote, and answer whether the quote closed.
///
/// In single quotes `\'` and `\\` are the escapes, and in double quotes
/// `\"`, `\\`, and `\$`. Any other backslash stands as itself.
fn read_quoted(chars: &mut Peekable<Chars<'_>>, quote: char, word: &mut String) -> bool {
    let escapes: &[char] = match quote {
        '\'' => &['\'', '\\'],
        _ => &['"', '\\', '$'],
    };
    while let Some(c) = chars.next() {
        if c == quote {
            return true;
        }
        match chars.next_if(|next| c == '\\' && escapes.contains(next)) {
            Some(escaped) => word.push(escaped),
            None => word.push(c),
        }
    }
    false
}

/// The options of `words`, one fish statement, when it is a `complete`
/// statement.
///
/// `-c`, `-n`, `-s`, `-l`, `-d`, `-a`, `-o`, and `-w`, and their long
/// spellings, each take the next word, and the first word of each counts.
/// `-r` and `-x` mark a value, and every other option takes no word.
fn read_complete(words: &[String]) -> Option<Complete<'_>> {
    let (first, options) = words.split_first()?;
    if first != "complete" {
        return None;
    }

    let mut complete = Complete::default();
    let mut unread = None;
    let mut options = options.iter();
    while let Some(option) = options.next() {
        let slot = match option.as_str() {
            "-r" | "--require-parameter" | "-x" | "--exclusive" => {
                complete.takes_value = true;
                continue;
            },
            "-c" | "--command" => &mut complete.command,
            "-n" | "--condition" => &mut complete.condition,
            "-s" | "--short-option" => &mut complete.short,
            "-l" | "--long-option" => &mut complete.long,
            "-d" | "--description" => &mut complete.description,
            "-a" | "--arguments" => &mut complete.arguments,
            "-o" | "--old-option" | "-w" | "--wraps" => &mut unread,
            _ => continue,
        };
        let Some(value) = options.next() else {
            break;
        };
        slot.get_or_insert(value.as_str());
    }
    Some(complete)
}

/// Whether the fish condition `condition` holds only inside a subcommand, as
/// clap's `__fish_<tool>_using_subcommand` and fish's
/// `__fish_seen_subcommand_from` test.
fn names_subcommand(condition: &str) -> bool {
    condition.contains("using_subcommand") || condition.contains("seen_subcommand_from")
}

/// The first sentence of `text`, cut at its first `. `, with no closing
/// period.
fn first_sentence(text: &str) -> String {
    let sentence = text.split_once(". ").map_or(text, |(head, _)| head);
    sentence.strip_suffix('.').unwrap_or(sentence).to_owned()
}

/// `text` split at its first run of two or more spaces, into the text before
/// the run and the text after it.
fn split_columns(text: &str) -> (&str, &str) {
    match text.find("  ") {
        Some(at) => (&text[..at], text[at..].trim()),
        None => (text.trim_end(), ""),
    }
}

/// The count of leading spaces of `line`.
fn indent(line: &str) -> usize {
    line.len() - text_of(line).len()
}

/// `line` with its leading spaces removed.
fn text_of(line: &str) -> &str {
    line.trim_start_matches(' ')
}

/// The help of ripgrep 13, in its usage and option-section shape.
#[cfg(test)]
pub(crate) const RG_HELP: &str = concat!(
    "USAGE:\n",
    "    rg [OPTIONS] PATTERN [PATH ...]\n",
    "    rg [OPTIONS] -e PATTERN ... [PATH ...]\n",
    "\n",
    "INPUT OPTIONS:\n",
    "    -e PATTERN, --regexp=PATTERN\n",
    "        A pattern to search for. This option can be provided multiple times,\n",
    "        where all patterns given are searched, in addition to any patterns\n",
    "\n",
    "SEARCH OPTIONS:\n",
    "    -i, --ignore-case\n",
    "        When this flag is provided, all patterns will be searched case\n",
    "        insensitively. The case insensitivity rules used by ripgrep's default\n",
    "\n",
    "    --mmap\n",
    "        When enabled, ripgrep will search using memory maps when possible. This\n",
    "        is enabled by default when ripgrep thinks it will be faster.\n",
    "\n",
    "FILTER OPTIONS:\n",
    "    -g GLOB, --glob=GLOB\n",
    "        Include or exclude files and directories for searching that match the\n",
    "        given glob. This always overrides any other ignore logic. Multiple glob\n",
    "\n",
    "    -., --hidden\n",
    "        Search hidden files and directories. By default, hidden files and\n",
    "        directories are skipped. Note that if a hidden file or a directory is",
);

/// The help of cargo, in clap's short shape with a commands section.
#[cfg(test)]
pub(crate) const CARGO_HELP: &str = concat!(
    "Rust's package manager\n",
    "\n",
    "Usage: cargo [OPTIONS] [COMMAND]\n",
    "       cargo [OPTIONS] -Zscript <MANIFEST_RS> [ARGS]...\n",
    "\n",
    "Options:\n",
    "  -V, --version                  Print version info and exit\n",
    "      --list                     List installed commands\n",
    "      --explain <CODE>           Provide a detailed explanation of a rustc error message\n",
    "  -v, --verbose...               Use verbose output (-vv very verbose/build.rs output)\n",
    "      --color <WHEN>             Coloring [possible values: auto, always, never]\n",
    "  -C <DIRECTORY>                 Change to DIRECTORY before doing anything (nightly-only)\n",
    "  -Z <FLAG>                      Unstable (nightly-only) flags to Cargo, see 'cargo -Z help' for\n",
    "                                 details\n",
    "  -h, --help                     Print help\n",
    "\n",
    "Commands:\n",
    "    build, b    Compile the current package\n",
    "    test, t     Run the tests\n",
    "    new         Create a new cargo package\n",
    "    ...         See all commands with --list\n",
    "\n",
    "See 'cargo help <command>' for more information on a specific command.",
);

/// The help of jj, in clap's long shape with each description under its flag.
#[cfg(test)]
const JJ_HELP: &str = concat!(
    "Jujutsu (An experimental VCS)\n",
    "\n",
    "Usage: jj [OPTIONS] <COMMAND>\n",
    "\n",
    "Commands:\n",
    "  abandon           Abandon a revision\n",
    "  bookmark          Manage bookmarks [default alias: b]\n",
    "  git               Commands for working with Git remotes and the underlying Git repo\n",
    "  log               Show revision history\n",
    "\n",
    "Options:\n",
    "  -h, --help\n",
    "          Print help (see a summary with '-h')\n",
    "\n",
    "Global Options:\n",
    "  -R, --repository <REPOSITORY>\n",
    "          Path to repository to operate on\n",
    "          \n",
    "          By default, Jujutsu searches for the closest .jj/ directory in an ancestor of the current\n",
    "          working directory.\n",
    "\n",
    "      --ignore-working-copy\n",
    "          Don't snapshot the working copy, and don't update it",
);

/// The fish completions of jj as clap writes them, with a value that spans
/// lines in its quotes and two statements of a subcommand.
#[cfg(test)]
pub(crate) const JJ_FISH: &str = r#"function __fish_jj_needs_command
    set -l cmd (commandline -opc)
end

complete -c jj -n "__fish_jj_needs_command" -s R -l repository -d 'Path to repository to operate on' -r -f -a "(__fish_complete_directories)"
complete -c jj -n "__fish_jj_needs_command" -l at-operation -l at-op -d 'Operation to load the repo at' -r
complete -c jj -n "__fish_jj_needs_command" -l color -d 'When to colorize output' -r -f -a "always\t''
never\t''
debug\t''
auto\t''"
complete -c jj -n "__fish_jj_needs_command" -l ignore-working-copy -d 'Don\'t snapshot the working copy, and don\'t update it'
complete -c jj -n "__fish_jj_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c jj -n "__fish_jj_needs_command" -f -a "git" -d 'Commands for working with Git remotes and the underlying Git repo'
complete -c jj -n "__fish_jj_needs_command" -f -a "log" -d 'Show revision history'
complete -c jj -n "__fish_jj_using_subcommand abandon" -s r -r
complete -c jj -n "__fish_jj_using_subcommand abandon" -l at-operation -l at-op -d 'Operation to load the repo at' -r"#;

#[cfg(test)]
mod tests {
    use super::{
        parse_fish_completions, parse_help, usage_signature, CliFlag, CliSpec, CliSubcommand,
        CARGO_HELP, JJ_FISH, JJ_HELP, RG_HELP,
    };

    /// The flag with the short form `short`, the long form `long`, the value
    /// `value`, and the description `description`.
    fn flag(
        short: Option<char>,
        long: Option<&str>,
        value: Option<&str>,
        description: &str,
    ) -> CliFlag {
        CliFlag {
            short,
            long: long.map(str::to_owned),
            value: value.map(str::to_owned),
            description: description.to_owned(),
        }
    }

    fn subcommand(name: &str, description: &str) -> CliSubcommand {
        CliSubcommand {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    #[test]
    fn ripgrep_help_gives_its_flags_and_usage() {
        assert_eq!(
            parse_help(RG_HELP),
            CliSpec {
                usage: Some("[flags] <pattern> [path...]".to_owned()),
                flags: vec![
                    flag(
                        Some('e'),
                        Some("regexp"),
                        Some("PATTERN"),
                        "A pattern to search for"
                    ),
                    flag(
                        Some('i'),
                        Some("ignore-case"),
                        None,
                        "When this flag is provided, all patterns will be searched case \
                         insensitively"
                    ),
                    flag(
                        None,
                        Some("mmap"),
                        None,
                        "When enabled, ripgrep will search using memory maps when possible"
                    ),
                    flag(
                        Some('g'),
                        Some("glob"),
                        Some("GLOB"),
                        "Include or exclude files and directories for searching that match the \
                         given glob"
                    ),
                    flag(
                        Some('.'),
                        Some("hidden"),
                        None,
                        "Search hidden files and directories"
                    ),
                ],
                subcommands: Vec::new(),
            },
        );
    }

    /// A long-only flag four columns under the short column is a flag, a
    /// description goes on through its deeper line, and an alias follows its
    /// name. The `...` row of the commands section names no subcommand.
    #[test]
    fn cargo_help_gives_its_flags_and_subcommands() {
        assert_eq!(
            parse_help(CARGO_HELP),
            CliSpec {
                usage: Some("[flags] [command]".to_owned()),
                flags: vec![
                    flag(
                        Some('V'),
                        Some("version"),
                        None,
                        "Print version info and exit"
                    ),
                    flag(None, Some("list"), None, "List installed commands"),
                    flag(
                        None,
                        Some("explain"),
                        Some("CODE"),
                        "Provide a detailed explanation of a rustc error message"
                    ),
                    flag(
                        Some('v'),
                        Some("verbose"),
                        None,
                        "Use verbose output (-vv very verbose/build.rs output)"
                    ),
                    flag(
                        None,
                        Some("color"),
                        Some("WHEN"),
                        "Coloring [possible values: auto, always, never]"
                    ),
                    flag(
                        Some('C'),
                        None,
                        Some("DIRECTORY"),
                        "Change to DIRECTORY before doing anything (nightly-only)"
                    ),
                    flag(
                        Some('Z'),
                        None,
                        Some("FLAG"),
                        "Unstable (nightly-only) flags to Cargo, see 'cargo -Z help' for details"
                    ),
                    flag(Some('h'), Some("help"), None, "Print help"),
                ],
                subcommands: vec![
                    subcommand("build", "Compile the current package"),
                    subcommand("b", "alias for build"),
                    subcommand("test", "Run the tests"),
                    subcommand("t", "alias for test"),
                    subcommand("new", "Create a new cargo package"),
                ],
            },
        );
    }

    /// A description that stands under its flag ends at a line of spaces.
    #[test]
    fn jj_help_gives_its_subcommands_and_flags() {
        assert_eq!(
            parse_help(JJ_HELP),
            CliSpec {
                usage: Some("[flags] <command>".to_owned()),
                flags: vec![
                    flag(
                        Some('h'),
                        Some("help"),
                        None,
                        "Print help (see a summary with '-h')"
                    ),
                    flag(
                        Some('R'),
                        Some("repository"),
                        Some("REPOSITORY"),
                        "Path to repository to operate on"
                    ),
                    flag(
                        None,
                        Some("ignore-working-copy"),
                        None,
                        "Don't snapshot the working copy, and don't update it"
                    ),
                ],
                subcommands: vec![
                    subcommand("abandon", "Abandon a revision"),
                    subcommand("bookmark", "Manage bookmarks [default alias: b]"),
                    subcommand(
                        "git",
                        "Commands for working with Git remotes and the underlying Git repo"
                    ),
                    subcommand("log", "Show revision history"),
                ],
            },
        );
    }

    /// An options token, a dash token, and an alternation of flags each give
    /// one leading `[flags]`.
    #[test]
    fn a_usage_line_becomes_a_signature() {
        assert_eq!(
            [
                "ls [OPTION]... [FILE]...",
                "cargo [OPTIONS] -Zscript <MANIFEST_RS> [ARGS]...",
                "git [-v | --version] <command> [<args>]",
            ]
            .map(usage_signature),
            [
                "[flags] [file...]",
                "[flags] <manifest_rs> [args...]",
                "[flags] <command> [args]",
            ],
        );
    }

    #[test]
    fn text_with_no_help_shape_gives_an_empty_spec() {
        assert_eq!(
            parse_help("A tool that prints nothing useful.\n\n  It reads stdin.\n"),
            CliSpec::default(),
        );
    }

    /// A spec with no flag and no subcommand, as a script that declares none
    /// for its tool gives.
    fn empty_fish_spec(flags: Vec<CliFlag>) -> CliSpec {
        CliSpec {
            usage: Some("[flags] [args...]".to_owned()),
            flags,
            subcommands: Vec::new(),
        }
    }

    /// The first `-l` names the flag, `-r` gives it a value, and a quoted
    /// value that spans lines ends where its quote closes.
    #[test]
    fn a_clap_fish_script_gives_the_tool_s_flags_and_subcommands() {
        assert_eq!(
            parse_fish_completions(JJ_FISH, "jj"),
            CliSpec {
                usage: Some("[flags] <command>".to_owned()),
                flags: vec![
                    flag(
                        Some('R'),
                        Some("repository"),
                        Some("VALUE"),
                        "Path to repository to operate on"
                    ),
                    flag(
                        None,
                        Some("at-operation"),
                        Some("VALUE"),
                        "Operation to load the repo at"
                    ),
                    flag(
                        None,
                        Some("color"),
                        Some("VALUE"),
                        "When to colorize output"
                    ),
                    flag(
                        None,
                        Some("ignore-working-copy"),
                        None,
                        "Don't snapshot the working copy, and don't update it"
                    ),
                    flag(
                        Some('h'),
                        Some("help"),
                        None,
                        "Print help (see more with '--help')"
                    ),
                ],
                subcommands: vec![
                    subcommand(
                        "git",
                        "Commands for working with Git remotes and the underlying Git repo"
                    ),
                    subcommand("log", "Show revision history"),
                ],
            },
        );
    }

    #[test]
    fn a_statement_for_another_command_is_skipped() {
        assert_eq!(
            parse_fish_completions(JJ_FISH, "git"),
            empty_fish_spec(Vec::new())
        );
    }

    /// A condition that holds inside a subcommand skips its statement, and
    /// one that holds before any subcommand keeps it.
    #[test]
    fn a_condition_on_a_subcommand_is_skipped() {
        let flags = |script| parse_fish_completions(script, "x").flags;
        assert_eq!(
            [
                flags(r#"complete -c x -n "__fish_seen_subcommand_from log" -l graph"#),
                flags(r#"complete -c x -n "__fish_use_subcommand" -l graph"#),
            ],
            [Vec::new(), vec![flag(None, Some("graph"), None, "")]],
        );
    }

    #[test]
    fn an_unclosed_quote_ends_the_script_and_gives_what_came_before() {
        let script = concat!(
            "complete -c x -l one\n",
            "complete -c x -l two -d 'never closed\n",
            "complete -c x -l three",
        );
        assert_eq!(
            parse_fish_completions(script, "x"),
            empty_fish_spec(vec![flag(None, Some("one"), None, "")]),
        );
    }
}
