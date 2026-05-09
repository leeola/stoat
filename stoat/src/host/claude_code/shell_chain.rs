//! Bash command-chain extraction for the [`super::denial`] gate.
//!
//! Splits a shell command into the sequence of simple commands that
//! `bash` would invoke. Lets the deny check inspect every link in a
//! chain so `cd /tmp && rm -rf /` triggers the same denial as bare
//! `rm -rf /` -- without the parser, the existing token-based check
//! only sees the head of the chain (`cd`).
//!
//! Modeled after Zed's `shell_command_parser` crate
//! (`references/zed/crates/shell_command_parser/src/shell_command_parser.rs`).
//! Narrower scope: this module returns just the sequence of normalized
//! command-line strings; redirect targets and process substitution
//! bodies are not extracted because the deny patterns we care about
//! match command names + args, not file targets.
//!
//! Quoting is stripped before matching so `rm -rf '/'` and `rm -rf /`
//! both normalize to `rm -rf /`. Parameter expansions like `$HOME`
//! retain their raw source text because the deny list matches `$HOME`
//! literally; expanding them would defeat the rule.

use brush_parser::{
    ast::{
        AndOr, AndOrList, Command, CommandPrefix, CommandPrefixOrSuffixItem, CommandSuffix,
        CompoundCommand, CompoundList, Pipeline, Program, SimpleCommand, Word,
    },
    word::{WordPiece, WordPieceWithSource},
    Parser, ParserOptions,
};

/// Returns the simple commands (in execution order) that `input` would
/// run as a bash script. A return of `None` signals that the parser
/// could not understand the input -- the caller should fall back to
/// inspecting the raw command rather than silently allowing it.
///
/// Simple commands embedded inside compound commands (subshells,
/// brace groups, if/while/for/case bodies, coprocesses) are surfaced
/// alongside top-level chains so a deny pattern catches
/// `(cd /tmp; rm -rf /)` and `if true; then rm -rf /; fi` the same
/// way as `cd /tmp && rm -rf /`.
///
/// Bodies of nested shell invocations (`bash -c '...'`, `eval '...'`)
/// are NOT recursively parsed -- the body is just an argument to
/// `bash` or `eval` from this parser's perspective, so a
/// `bash -c 'rm -rf /'` invocation surfaces as one command
/// (`bash -c rm -rf /`) and only matches deny patterns whose head is
/// `bash`. Recursion into shell-invocation arguments is a known v1
/// limitation.
pub(crate) fn extract_simple_commands(input: &str) -> Option<Vec<String>> {
    let options = ParserOptions::default();
    let mut parser = Parser::new(std::io::BufReader::new(input.as_bytes()), &options);
    let program = parser.parse_program().ok()?;

    let mut commands = Vec::new();
    walk_program(&program, &mut commands)?;
    Some(commands)
}

fn walk_program(program: &Program, out: &mut Vec<String>) -> Option<()> {
    for complete in &program.complete_commands {
        walk_compound_list(complete, out)?;
    }
    Some(())
}

fn walk_compound_list(list: &CompoundList, out: &mut Vec<String>) -> Option<()> {
    for item in &list.0 {
        walk_and_or_list(&item.0, out)?;
    }
    Some(())
}

fn walk_and_or_list(list: &AndOrList, out: &mut Vec<String>) -> Option<()> {
    walk_pipeline(&list.first, out)?;
    for and_or in &list.additional {
        let pipeline = match and_or {
            AndOr::And(p) | AndOr::Or(p) => p,
        };
        walk_pipeline(pipeline, out)?;
    }
    Some(())
}

fn walk_pipeline(pipeline: &Pipeline, out: &mut Vec<String>) -> Option<()> {
    for command in &pipeline.seq {
        walk_command(command, out)?;
    }
    Some(())
}

fn walk_command(command: &Command, out: &mut Vec<String>) -> Option<()> {
    match command {
        Command::Simple(simple) => {
            if let Some(s) = build_simple_command(simple)? {
                out.push(s);
            }
        },
        Command::Compound(compound, _redirects) => walk_compound_command(compound, out)?,
        Command::Function(func) => walk_compound_command(&func.body.0, out)?,
        Command::ExtendedTest(_, _) => {},
    }
    Some(())
}

fn walk_compound_command(compound: &CompoundCommand, out: &mut Vec<String>) -> Option<()> {
    match compound {
        CompoundCommand::BraceGroup(g) => walk_compound_list(&g.list, out)?,
        CompoundCommand::Subshell(s) => walk_compound_list(&s.list, out)?,
        CompoundCommand::ForClause(f) => walk_compound_list(&f.body.list, out)?,
        CompoundCommand::CaseClause(c) => {
            for case in &c.cases {
                if let Some(body) = &case.cmd {
                    walk_compound_list(body, out)?;
                }
            }
        },
        CompoundCommand::IfClause(i) => {
            walk_compound_list(&i.condition, out)?;
            walk_compound_list(&i.then, out)?;
            if let Some(elses) = &i.elses {
                for else_clause in elses {
                    if let Some(cond) = &else_clause.condition {
                        walk_compound_list(cond, out)?;
                    }
                    walk_compound_list(&else_clause.body, out)?;
                }
            }
        },
        CompoundCommand::WhileClause(w) | CompoundCommand::UntilClause(w) => {
            walk_compound_list(&w.0, out)?;
            walk_compound_list(&w.1.list, out)?;
        },
        CompoundCommand::Coprocess(c) => walk_command(&c.body, out)?,
        CompoundCommand::Arithmetic(_) | CompoundCommand::ArithmeticForClause(_) => {},
    }
    Some(())
}

fn build_simple_command(simple: &SimpleCommand) -> Option<Option<String>> {
    let mut words: Vec<String> = Vec::new();
    if let Some(prefix) = &simple.prefix {
        collect_words_from_prefix(prefix, &mut words)?;
    }
    if let Some(name) = &simple.word_or_name {
        words.push(normalize_word(name)?);
    }
    if let Some(suffix) = &simple.suffix {
        collect_words_from_suffix(suffix, &mut words)?;
    }
    if words.is_empty() {
        return Some(None);
    }
    Some(Some(words.join(" ")))
}

fn collect_words_from_prefix(prefix: &CommandPrefix, out: &mut Vec<String>) -> Option<()> {
    for item in &prefix.0 {
        collect_word_from_prefix_or_suffix_item(item, out)?;
    }
    Some(())
}

fn collect_words_from_suffix(suffix: &CommandSuffix, out: &mut Vec<String>) -> Option<()> {
    for item in &suffix.0 {
        collect_word_from_prefix_or_suffix_item(item, out)?;
    }
    Some(())
}

fn collect_word_from_prefix_or_suffix_item(
    item: &CommandPrefixOrSuffixItem,
    out: &mut Vec<String>,
) -> Option<()> {
    match item {
        CommandPrefixOrSuffixItem::Word(word) => out.push(normalize_word(word)?),
        CommandPrefixOrSuffixItem::AssignmentWord(_, word) => out.push(normalize_word(word)?),
        CommandPrefixOrSuffixItem::IoRedirect(_)
        | CommandPrefixOrSuffixItem::ProcessSubstitution(_, _) => {},
    }
    Some(())
}

fn normalize_word(word: &Word) -> Option<String> {
    let options = ParserOptions::default();
    let pieces = brush_parser::word::parse(&word.value, &options).ok()?;
    let mut result = String::new();
    for piece in &pieces {
        normalize_word_piece(piece, &word.value, &mut result)?;
    }
    Some(result)
}

fn normalize_word_piece(piece: &WordPieceWithSource, raw: &str, out: &mut String) -> Option<()> {
    match &piece.piece {
        WordPiece::Text(s) | WordPiece::SingleQuotedText(s) | WordPiece::AnsiCQuotedText(s) => {
            out.push_str(s)
        },
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            for nested in inner {
                normalize_word_piece(nested, raw, out)?;
            }
        },
        WordPiece::EscapeSequence(s) => out.push_str(s.strip_prefix('\\').unwrap_or(s)),
        WordPiece::TildeExpansion(_)
        | WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_)
        | WordPiece::ArithmeticExpression(_) => {
            let slice = raw.get(piece.start_index..piece.end_index)?;
            out.push_str(slice);
        },
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(input: &str) -> Vec<String> {
        extract_simple_commands(input).expect("parse should succeed")
    }

    #[test]
    fn single_command() {
        assert_eq!(extract("rm -rf /"), vec!["rm -rf /"]);
    }

    #[test]
    fn and_chain() {
        assert_eq!(extract("cd /tmp && rm -rf /"), vec!["cd /tmp", "rm -rf /"]);
    }

    #[test]
    fn or_chain() {
        assert_eq!(
            extract("echo hi || rm -rf $HOME"),
            vec!["echo hi", "rm -rf $HOME"]
        );
    }

    #[test]
    fn pipe_chain() {
        assert_eq!(extract("ls | grep foo"), vec!["ls", "grep foo"]);
    }

    #[test]
    fn semicolon_sequence() {
        assert_eq!(extract("echo a; rm -rf /"), vec!["echo a", "rm -rf /"]);
    }

    #[test]
    fn subshell_recurses() {
        assert_eq!(extract("(cd /tmp; rm -rf /)"), vec!["cd /tmp", "rm -rf /"]);
    }

    #[test]
    fn brace_group_recurses() {
        assert_eq!(extract("{ echo a; rm -rf /; }"), vec!["echo a", "rm -rf /"]);
    }

    #[test]
    fn if_clause_recurses() {
        assert_eq!(
            extract("if true; then rm -rf /; fi"),
            vec!["true", "rm -rf /"]
        );
    }

    #[test]
    fn single_quotes_stripped() {
        assert_eq!(extract("rm -rf '/'"), vec!["rm -rf /"]);
    }

    #[test]
    fn double_quotes_stripped() {
        assert_eq!(extract(r#"rm -rf "/""#), vec!["rm -rf /"]);
    }

    #[test]
    fn parameter_expansion_preserved_in_double_quotes() {
        assert_eq!(extract(r#"rm -rf "$HOME""#), vec!["rm -rf $HOME"]);
    }

    #[test]
    fn quoted_chain_operators_are_arguments() {
        assert_eq!(extract(r#"echo "&& rm -rf /""#), vec!["echo && rm -rf /"]);
    }

    #[test]
    fn nested_shell_invocation_not_recursed() {
        assert_eq!(extract(r#"bash -c 'rm -rf /'"#), vec!["bash -c rm -rf /"]);
    }

    #[test]
    fn parse_failure_returns_none() {
        assert!(extract_simple_commands("if then fi").is_none());
    }
}
