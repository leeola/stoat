use crate::{
    command_palette,
    keymap::{ResolvedAction, ResolvedArg},
    keymap_state,
};
use std::{mem, sync::Arc};
use stoat_action::{registry, ParamDef};
use stoat_config::{
    Call, CallArg, CallValue, CommandBody, CommandDecl, CommandStatement, Config, ParseError,
    RunExpr, Span, Spanned, Template, Value,
};

pub(crate) mod context;
pub(crate) mod run;

/// The user commands that the config's `command` items declare.
///
/// A user command runs from the palette as a built-in command does. Its name
/// is unique in the set, ASCII case ignored. No built-in command, action, or
/// command group answers to it, so a user command never hides a built-in one.
#[derive(Default)]
pub(crate) struct CommandSet {
    commands: Vec<Arc<UserCommand>>,
}

/// A command that a `command NAME { ... }` item declares, after the loader
/// checks it.
#[derive(Debug, PartialEq)]
pub(crate) struct UserCommand {
    pub(crate) name: String,
    /// The text of the `doc` statement, empty for a command with none.
    pub(crate) doc: String,
    /// The line of the `command` keyword in the source that declares the
    /// command, counting from 1. The palette names it as where the command is
    /// defined.
    pub(crate) line: u32,
    /// The arguments in the order that the palette takes them. An argument
    /// with a default is optional, and every other one is required.
    pub(crate) args: Vec<CommandArg>,
    pub(crate) step: Step,
    pub(crate) output: Output,
}

/// An argument of a [`UserCommand`], which a `{name}` placeholder of its step
/// names.
#[derive(Debug, PartialEq)]
pub(crate) struct CommandArg {
    pub(crate) name: String,
    pub(crate) kind: ArgKind,
}

/// What an argument of a [`UserCommand`] takes. The kind sets the list that
/// the palette shows while the argument is in entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ArgKind {
    /// Typed text, with no list. An argument with a default is optional, and
    /// an empty value of it takes the default when the command runs.
    Text { default: Option<TextDefault> },
    /// A file, from the list of workspace files that `:o` shows.
    File,
    /// A directory, from the list of workspace directories that `:cd` shows.
    Dir,
}

/// The value that an empty `text` argument takes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TextDefault {
    /// The text of a string, a number, or a boolean literal.
    Literal(String),
    /// A value of the editor state, which a run reads when it starts.
    Context(ContextValue),
}

/// A value of the editor state that a default names. It comes from the pane
/// under the palette, or from the workspace's repository.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ContextValue {
    /// `git.last_subject`, the summary of the newest commit.
    GitLastSubject,
    /// `selection`, the text of the primary selection.
    Selection,
    /// `file`, the path of the pane's buffer.
    File,
    /// `line`, the 1-based line of the primary cursor.
    Line,
}

/// What a [`UserCommand`] runs.
#[derive(Debug, PartialEq)]
pub(crate) enum Step {
    /// A shell line, with each value written as one shell word.
    Shell(Template),
    /// An action, with each placeholder of a string argument written as it is.
    Action(ResolvedAction),
}

/// Where the output of a [`UserCommand`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// The run modal, which shows the output of a shell step.
    Block,
    /// A status message, with the first line of the output or the failure.
    Notify,
    /// A scratch buffer that holds the whole stdout of a run that worked. A
    /// failure shows its message and opens no buffer.
    Buffer,
    /// A Run pane, whose shell runs the line of a shell step.
    RunPane,
    /// The location picker, over the `path:line:col: text` lines of the
    /// stdout, whatever the exit code, because a search tool exits 1 when it
    /// finds nothing.
    Locations,
}

/// A command as its layer lowers it, with what the `same_as` check reads.
struct Lowered {
    command: UserCommand,
    same_as: Vec<SameAs>,
    /// Whether the user config declares the command. The errors of the
    /// embedded config go to the log, not to the user.
    user: bool,
}

/// An argument of kind `same_as(target)`, which takes the kind of the first
/// argument of the command `target`.
struct SameAs {
    /// The place of the argument among the arguments of its command.
    index: usize,
    target: String,
    span: Span,
}

impl CommandSet {
    /// Lower the `command` items of `embedded` and of `user` into one set.
    ///
    /// Each layer comes with the source that it parses from, which gives each
    /// command the line of its declaration.
    ///
    /// A user command takes the place of an embedded command of its name. A
    /// declaration that fails a check is left out, and the others load. The
    /// errors of `embedded` go to the log, and the answer holds the errors of
    /// `user`, each at the span of its fault.
    pub(crate) fn build(
        embedded: (&Config, &str),
        user: Option<(&Config, &str)>,
    ) -> (CommandSet, Vec<ParseError>) {
        let mut embedded_errors = Vec::new();
        let mut user_errors = Vec::new();
        let mut lowered = lower_layer(embedded, false, &mut embedded_errors);
        if let Some(user) = user {
            for command in lower_layer(user, true, &mut user_errors) {
                lowered.retain(|held| {
                    !held
                        .command
                        .name
                        .eq_ignore_ascii_case(&command.command.name)
                });
                lowered.push(command);
            }
        }
        check_same_as(&mut lowered, &mut embedded_errors, &mut user_errors);
        resolve_same_as(&mut lowered);

        for error in &embedded_errors {
            tracing::error!(target: "stoat::config", "shipped config: {}", error.message);
        }
        let commands = lowered
            .into_iter()
            .map(|lowered| Arc::new(lowered.command))
            .collect();
        (CommandSet { commands }, user_errors)
    }

    /// The command named `name`, ASCII case ignored.
    pub(crate) fn get(&self, name: &str) -> Option<&Arc<UserCommand>> {
        self.commands
            .iter()
            .find(|command| command.name.eq_ignore_ascii_case(name))
    }

    /// The commands in declaration order, the embedded layer's first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<UserCommand>> {
        self.commands.iter()
    }
}

impl UserCommand {
    /// The arguments as the palette's usage line writes them, one space apart.
    /// A required argument reads `<name>`, and an optional one `[name]`.
    pub(crate) fn signature(&self) -> String {
        let args: Vec<String> = self
            .args
            .iter()
            .map(|arg| match arg.optional() {
                true => format!("[{}]", arg.name),
                false => format!("<{}>", arg.name),
            })
            .collect();
        args.join(" ")
    }
}

impl CommandArg {
    /// The default that an empty value of the argument takes, or `None` for an
    /// argument that the user must fill.
    pub(crate) fn default(&self) -> Option<&TextDefault> {
        match &self.kind {
            ArgKind::Text { default } => default.as_ref(),
            ArgKind::File | ArgKind::Dir => None,
        }
    }

    /// Whether a run takes the argument empty, which an argument with a
    /// default allows.
    pub(crate) fn optional(&self) -> bool {
        self.default().is_some()
    }
}

/// The commands of one config layer that pass their checks, with an error in
/// `errors` for each one that does not.
///
/// The later of two declarations of one name is an error, ASCII case ignored,
/// even when the first one fails a check of its own.
fn lower_layer(
    (config, source): (&Config, &str),
    user: bool,
    errors: &mut Vec<ParseError>,
) -> Vec<Lowered> {
    let mut declared: Vec<&str> = Vec::new();
    let mut lowered = Vec::new();
    for decl in &config.commands {
        let name = &decl.node.name;
        if let Some(message) = taken(&name.node) {
            errors.push(error(&name.span, message));
            continue;
        }
        if declared
            .iter()
            .any(|held| held.eq_ignore_ascii_case(&name.node))
        {
            errors.push(error(
                &name.span,
                format!("command `{}` is declared twice", name.node),
            ));
            continue;
        }
        declared.push(&name.node);

        let before = source.get(..decl.span.start).unwrap_or(source);
        let line = before.bytes().filter(|&byte| byte == b'\n').count() as u32 + 1;
        if let Some((command, same_as)) = lower(&decl.node, source, line, errors) {
            lowered.push(Lowered {
                command,
                same_as,
                user,
            });
        }
    }
    lowered
}

/// Why `name` is no name for a user command, or `None` when it is free.
///
/// A name that an action answers to is taken, a command or not. The palette
/// reads a saved history line whose first word names an internal action as
/// that action.
fn taken(name: &str) -> Option<String> {
    let built_in = registry::lookup_alias(name)
        .map(|entry| entry.command_name.clone())
        .or_else(|| {
            let token = command_palette::is_conditional_token(name)
                || name.eq_ignore_ascii_case(&command_palette::action_row().command_name);
            token.then(|| name.to_owned())
        });
    if let Some(built_in) = built_in {
        return Some(format!(
            "command `{name}` shadows the built-in command `{built_in}`"
        ));
    }
    registry::group_of(name)
        .map(|group| format!("command `{name}` shadows the command group `{group}`"))
}

/// `decl` as a command and its `same_as` arguments, or `None` with an error
/// in `errors` for each fault of the declaration.
///
/// `source` is the text that `decl` parses from, which keeps the literal text
/// of a default.
fn lower(
    decl: &CommandDecl,
    source: &str,
    line: u32,
    errors: &mut Vec<ParseError>,
) -> Option<(UserCommand, Vec<SameAs>)> {
    let statements = match &decl.body {
        CommandBody::Block(statements) => statements,
        CommandBody::Wrap(call) => {
            let form = &call.node.name;
            errors.push(error(
                &form.span,
                format!("`{}` is not a command form", form.node),
            ));
            return None;
        },
    };

    let faults = errors.len();
    let mut args: Vec<CommandArg> = Vec::new();
    let mut same_as = Vec::new();
    let mut run: Option<Option<&Spanned<Call>>> = None;
    let mut output: Option<Option<Output>> = None;
    let mut doc: Option<String> = None;
    for statement in statements {
        let twice = |key: &str| error(&statement.span, format!("`{key}` is set twice"));
        match &statement.node {
            CommandStatement::Doc(_) if doc.is_some() => errors.push(twice("doc")),
            CommandStatement::Doc(text) => doc = Some(text.node.clone()),
            CommandStatement::Arg {
                name,
                kind: declared,
            } => {
                if args.iter().any(|arg| arg.name == name.node) {
                    errors.push(error(
                        &name.span,
                        format!("argument `{}` is declared twice", name.node),
                    ));
                }
                let kind = match arg_kind(declared, source) {
                    Ok((kind, Some(target))) => {
                        same_as.push(SameAs {
                            index: args.len(),
                            target,
                            span: declared.span.clone(),
                        });
                        kind
                    },
                    Ok((kind, None)) => kind,
                    Err(fault) => {
                        errors.push(fault);
                        ArgKind::Text { default: None }
                    },
                };
                args.push(CommandArg {
                    name: name.node.clone(),
                    kind,
                });
            },
            CommandStatement::Run(_) if run.is_some() => errors.push(twice("run")),
            CommandStatement::Run(RunExpr::Single(call)) => run = Some(Some(call)),
            CommandStatement::Run(RunExpr::Sequence(_)) => {
                errors.push(error(&statement.span, "`run` takes one step"));
                run = Some(None);
            },
            CommandStatement::Output(_) if output.is_some() => errors.push(twice("output")),
            CommandStatement::Output(value) => {
                output = Some(match value.node.as_str() {
                    "block" => Some(Output::Block),
                    "notify" => Some(Output::Notify),
                    "buffer" => Some(Output::Buffer),
                    "run_pane" => Some(Output::RunPane),
                    "locations" => Some(Output::Locations),
                    other => {
                        errors.push(error(&value.span, format!("unknown output `{other}`")));
                        None
                    },
                });
            },
        }
    }

    let name = &decl.name.node;
    let step = match run {
        None => {
            errors.push(error(
                &decl.name.span,
                format!("command `{name}` has no run"),
            ));
            None
        },
        Some(call) => call.and_then(|call| lower_step(call, name, &args, errors)),
    };
    if errors.len() > faults {
        return None;
    }

    let command = UserCommand {
        name: name.clone(),
        doc: doc.unwrap_or_default(),
        line,
        args,
        step: step?,
        output: output.flatten().unwrap_or(Output::Block),
    };
    Some((command, same_as))
}

/// The kind that the argument kind `kind` names, with the target of a
/// `same_as(target)` kind, or why `kind` is no argument kind.
///
/// A `same_as` argument takes [`ArgKind::Text`] until [`resolve_same_as`]
/// gives it the kind of its target's first argument.
fn arg_kind(kind: &Spanned<Call>, source: &str) -> Result<(ArgKind, Option<String>), ParseError> {
    let name = &kind.node.name;
    let bare = |arg_kind: ArgKind| match kind.node.args.as_deref() {
        None | Some([]) => Ok((arg_kind, None)),
        Some(_) => Err(error(
            &kind.span,
            format!("`{}` takes no argument", name.node),
        )),
    };
    match name.node.as_str() {
        "text" => text_kind(kind, source).map(|kind| (kind, None)),
        "file" => bare(ArgKind::File),
        "dir" => bare(ArgKind::Dir),
        "same_as" => match kind.node.args.as_deref() {
            Some(
                [Spanned {
                    node:
                        CallArg::Positional(Spanned {
                            node: CallValue::Value(Value::Ident(target)),
                            ..
                        }),
                    ..
                }],
            ) => Ok((ArgKind::Text { default: None }, Some(target.clone()))),
            _ => Err(error(&kind.span, "`same_as` takes one command name")),
        },
        other => Err(error(
            &name.span,
            format!("unknown argument kind `{other}`"),
        )),
    }
}

/// The kind that the call `text(...)` names, with the default of its
/// `default:` argument, or why the call names no `text` kind.
fn text_kind(call: &Spanned<Call>, source: &str) -> Result<ArgKind, ParseError> {
    let mut default = None;
    for arg in call.node.args.iter().flatten() {
        match &arg.node {
            CallArg::Named { name, value } if name.node == "default" => {
                if default.is_some() {
                    return Err(error(&arg.span, "`default` is set twice"));
                }
                default = Some(text_default(value, source)?);
            },
            CallArg::Named { name, .. } => {
                return Err(error(
                    &name.span,
                    format!("`text` takes no argument `{}`", name.node),
                ));
            },
            CallArg::Positional(_) => return Err(error(&call.span, "`text` takes no argument")),
        }
    }
    Ok(ArgKind::Text { default })
}

/// The default that the value `value` of a `default:` argument names.
///
/// A string, a number, or a boolean is a literal, which keeps the text of a
/// number or a boolean as `source` writes it. The names `selection`, `file`,
/// and `line` and the dotted name `git.last_subject` are context values. Any
/// other value is an error.
fn text_default(value: &Spanned<CallValue>, source: &str) -> Result<TextDefault, ParseError> {
    let written = source.get(value.span.clone()).unwrap_or_default();
    let context = match &value.node {
        CallValue::Value(Value::String(text)) => return Ok(TextDefault::Literal(text.clone())),
        CallValue::Value(Value::Number(_) | Value::Bool(_)) => {
            return Ok(TextDefault::Literal(written.to_owned()));
        },
        CallValue::Value(Value::Ident(name)) => match name.as_str() {
            "selection" => Some(ContextValue::Selection),
            "file" => Some(ContextValue::File),
            "line" => Some(ContextValue::Line),
            _ => None,
        },
        CallValue::Context(path) => {
            let names: Vec<&str> = path.iter().map(|segment| segment.node.as_str()).collect();
            (names == ["git", "last_subject"]).then_some(ContextValue::GitLastSubject)
        },
        CallValue::Value(_) => None,
    };
    context
        .map(TextDefault::Context)
        .ok_or_else(|| error(&value.span, format!("unknown context value `{written}`")))
}

/// The step that `call` runs for the command `command`, or `None` with an
/// error in `errors` for each fault of the step.
///
/// A `shell` call takes one string. Any other call names an action, which
/// must run as a key binding of it does. It takes only the arguments that the
/// action declares, each at the place of its parameter. A placeholder in the
/// shell line or in a string argument of the action names an argument of the
/// command.
fn lower_step(
    call: &Spanned<Call>,
    command: &str,
    args: &[CommandArg],
    errors: &mut Vec<ParseError>,
) -> Option<Step> {
    let name = &call.node.name.node;
    let Some(call_args) = call.node.args.as_deref() else {
        errors.push(error(&call.span, format!("`{name}` is not a step")));
        return None;
    };

    if name == "shell" {
        let template = match call_args {
            [Spanned {
                node:
                    CallArg::Positional(Spanned {
                        node: CallValue::Value(Value::String(line)),
                        ..
                    }),
                ..
            }] => Template::parse(line),
            _ => {
                errors.push(error(&call.span, "`shell` takes one string"));
                return None;
            },
        };
        return check_placeholders(&[&template], call, command, args, errors)
            .then_some(Step::Shell(template));
    }

    let mut resolved = Vec::with_capacity(call_args.len());
    for arg in call_args {
        let (arg_name, value) = match &arg.node {
            CallArg::Positional(value) => (None, value),
            CallArg::Named { name, value } => (Some(name.node.clone()), value),
        };
        let CallValue::Value(value) = &value.node else {
            errors.push(error(
                &value.span,
                "an action argument takes no context value",
            ));
            return None;
        };
        resolved.push(ResolvedArg {
            name: arg_name,
            value: value.clone(),
        });
    }
    let action = ResolvedAction {
        name: name.clone(),
        args: resolved,
    };
    if let Err(reason) = keymap_state::check_bound_action(&action) {
        errors.push(error(&call.span, reason));
        return None;
    }
    if let Some(entry) = registry::lookup(name)
        && !check_params(entry.def.params(), name, call_args, errors)
    {
        return None;
    }

    let templates: Vec<Template> = action
        .args
        .iter()
        .filter_map(|arg| match &arg.value {
            Value::String(text) => Some(Template::parse(text)),
            _ => None,
        })
        .collect();
    let templates: Vec<&Template> = templates.iter().collect();
    check_placeholders(&templates, call, command, args, errors).then_some(Step::Action(action))
}

/// Whether each argument of the action `action` stands at the place of one
/// of `params`.
///
/// Each argument that does not gets an error in `errors`. An action reads its
/// arguments by place. So a named argument names the parameter of its own
/// place, and the action never reads an argument past the last parameter.
fn check_params(
    params: &[ParamDef],
    action: &str,
    call_args: &[Spanned<CallArg>],
    errors: &mut Vec<ParseError>,
) -> bool {
    let faults = errors.len();
    for (place, arg) in call_args.iter().enumerate() {
        let Some(param) = params.get(place) else {
            let takes = match params.len() {
                0 => "takes no argument".to_owned(),
                1 => "takes 1 argument".to_owned(),
                count => format!("takes {count} arguments"),
            };
            errors.push(error(&arg.span, format!("action `{action}` {takes}")));
            continue;
        };
        if let CallArg::Named { name, .. } = &arg.node
            && name.node != param.name
        {
            let message = match params.iter().any(|param| param.name == name.node) {
                true => format!(
                    "argument `{}` of action `{action}` is not at its parameter's place",
                    name.node
                ),
                false => format!("action `{action}` declares no parameter `{}`", name.node),
            };
            errors.push(error(&name.span, message));
        }
    }
    errors.len() == faults
}

/// Whether each placeholder of `templates` names an argument of the command
/// `command`, with an error at `call` in `errors` for each name that does
/// not.
fn check_placeholders(
    templates: &[&Template],
    call: &Spanned<Call>,
    command: &str,
    args: &[CommandArg],
    errors: &mut Vec<ParseError>,
) -> bool {
    let mut unknown: Vec<&str> = templates
        .iter()
        .flat_map(|template| template.placeholders())
        .filter(|name| !args.iter().any(|arg| arg.name == *name))
        .collect();
    unknown.sort_unstable();
    unknown.dedup();
    for name in &unknown {
        errors.push(error(
            &call.span,
            format!("placeholder `{{{name}}}` names no argument of `{command}`"),
        ));
    }
    unknown.is_empty()
}

/// Leave out each command of `lowered` with a `same_as` argument that fails.
///
/// A failed argument names no command with an argument, or starts a chain of
/// first arguments that loops. The checks repeat until no command fails,
/// because a command that leaves the set takes the target of another
/// `same_as` with it.
fn check_same_as(
    lowered: &mut Vec<Lowered>,
    embedded_errors: &mut Vec<ParseError>,
    user_errors: &mut Vec<ParseError>,
) {
    loop {
        let faults: Vec<Option<ParseError>> = lowered
            .iter()
            .map(|held| {
                held.same_as
                    .iter()
                    .find_map(|link| same_as_fault(lowered, link))
            })
            .collect();
        if faults.iter().all(Option::is_none) {
            return;
        }

        for (held, fault) in mem::take(lowered).into_iter().zip(faults) {
            match (fault, held.user) {
                (None, _) => lowered.push(held),
                (Some(fault), true) => user_errors.push(fault),
                (Some(fault), false) => embedded_errors.push(fault),
            }
        }
    }
}

/// Why `link` names no kind, or `None` when its chain of first arguments
/// ends on a kind.
///
/// The chain visits a command when it reads that command's first argument,
/// so a chain that comes back to a command it visited loops. A link of the
/// chain that names no command is the fault of the command that holds it.
fn same_as_fault(lowered: &[Lowered], link: &SameAs) -> Option<ParseError> {
    let find = |name: &str| {
        lowered
            .iter()
            .find(|held| held.command.name.eq_ignore_ascii_case(name))
    };
    let Some(target) = find(&link.target) else {
        return Some(error(
            &link.span,
            format!("`same_as({})` names no command", link.target),
        ));
    };
    if target.command.args.is_empty() {
        return Some(error(
            &link.span,
            format!("command `{}` has no argument", link.target),
        ));
    }

    let mut visited: Vec<&str> = Vec::new();
    let mut at = target;
    loop {
        if visited.contains(&at.command.name.as_str()) {
            return Some(error(
                &link.span,
                format!("`same_as({})` forms a cycle", link.target),
            ));
        }
        visited.push(&at.command.name);
        let next = at.same_as.iter().find(|link| link.index == 0)?;
        at = find(&next.target)?;
    }
}

/// Give each `same_as` argument of `lowered` the kind of the first argument at
/// the end of its chain.
///
/// [`check_same_as`] runs first, so each chain that is left ends on a kind.
fn resolve_same_as(lowered: &mut [Lowered]) {
    let kinds: Vec<(usize, usize, ArgKind)> = {
        let view: &[Lowered] = lowered;
        view.iter()
            .enumerate()
            .flat_map(|(at, held)| {
                held.same_as
                    .iter()
                    .filter_map(move |link| Some((at, link.index, chain_kind(view, link)?)))
            })
            .collect()
    };
    for (at, index, kind) in kinds {
        lowered[at].command.args[index].kind = kind;
    }
}

/// The kind of the first argument at the end of `link`'s chain of first
/// arguments, or `None` for a chain that ends on no kind.
///
/// A chain that ends visits each command once at most. So a walk longer than
/// the set loops, and gives `None` as a chain that names no command does.
fn chain_kind(lowered: &[Lowered], link: &SameAs) -> Option<ArgKind> {
    let find = |name: &str| {
        lowered
            .iter()
            .find(|held| held.command.name.eq_ignore_ascii_case(name))
    };
    let mut at = find(&link.target)?;
    for _ in 0..lowered.len() {
        match at.same_as.iter().find(|first| first.index == 0) {
            Some(next) => at = find(&next.target)?,
            None => return at.command.args.first().map(|arg| arg.kind.clone()),
        }
    }
    None
}

fn error(span: &Span, message: impl Into<String>) -> ParseError {
    ParseError::new(span.clone(), message)
}

#[cfg(test)]
mod tests {
    use super::{
        ArgKind, CommandArg, CommandSet, ContextValue, Output, Step, TextDefault, UserCommand,
    };
    use crate::app;
    use stoat_config::{Config, Span, Template};

    /// The commands and the errors that `source` gives as the user layer over
    /// an embedded layer with no command.
    fn load(source: &str) -> (CommandSet, Vec<(Span, String)>) {
        let (set, errors) = CommandSet::build((&config(""), ""), Some((&config(source), source)));
        let errors = errors
            .into_iter()
            .map(|error| (error.span, error.message))
            .collect();
        (set, errors)
    }

    /// `source` parsed with no error.
    fn config(source: &str) -> Config {
        let (config, errors) = stoat_config::parse(source);
        assert_eq!(errors, Vec::new(), "{source:?} parses");
        config.expect("a parsed config")
    }

    /// The span of the first `needle` in `source`.
    fn span(source: &str, needle: &str) -> Span {
        let start = source
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} stands in the source"));
        start..start + needle.len()
    }

    /// The span of the name of the item `command NAME` in `source`.
    fn name_span(source: &str, name: &str) -> Span {
        let item = span(source, &format!("command {name} "));
        item.start + "command ".len()..item.start + "command ".len() + name.len()
    }

    /// The names of the commands of `set`, in order.
    fn names(set: &CommandSet) -> Vec<&str> {
        set.commands
            .iter()
            .map(|command| command.name.as_str())
            .collect()
    }

    /// `(span, message)` with the message owned.
    fn fault(span: Span, message: &str) -> (Span, String) {
        (span, message.to_owned())
    }

    /// The kinds of the arguments of the command `name` of `set`.
    fn kinds(set: &CommandSet, name: &str) -> Option<Vec<ArgKind>> {
        set.get(name)
            .map(|command| command.args.iter().map(|arg| arg.kind.clone()).collect())
    }

    #[test]
    fn a_block_command_lowers_to_its_arguments_step_and_output() {
        let (set, errors) = load(
            r#"command greet { doc = "say hello"; arg who = text; run = shell("echo hello {who}"); output = notify; }"#,
        );
        let commands: Vec<&UserCommand> = set.commands.iter().map(AsRef::as_ref).collect();
        assert_eq!(
            (commands, errors),
            (
                vec![&UserCommand {
                    name: "greet".to_owned(),
                    doc: "say hello".to_owned(),
                    line: 1,
                    args: vec![CommandArg {
                        name: "who".to_owned(),
                        kind: ArgKind::Text { default: None },
                    }],
                    step: Step::Shell(Template::parse("echo hello {who}")),
                    output: Output::Notify,
                }],
                Vec::new(),
            ),
        );
    }

    /// A name that a built-in command, an action, a command group, a
    /// conditional token, or the `action` row answers to is taken.
    #[test]
    fn a_taken_name_is_reported_at_the_name() {
        let taken = ["review", "q", "git", "next", "action"];
        let source = taken
            .map(|name| format!("command {name} {{ run = shell(\"x\"); }}\n"))
            .concat();
        let (set, errors) = load(&source);
        let at = |name: &str| name_span(&source, name);
        assert_eq!(
            (errors, taken.map(|name| set.get(name).is_none())),
            (
                vec![
                    fault(
                        at("review"),
                        "command `review` shadows the built-in command `review`"
                    ),
                    fault(at("q"), "command `q` shadows the built-in command `quit`"),
                    fault(at("git"), "command `git` shadows the command group `git`"),
                    fault(
                        at("next"),
                        "command `next` shadows the built-in command `next`"
                    ),
                    fault(
                        at("action"),
                        "command `action` shadows the built-in command `action`"
                    ),
                ],
                [true; 5],
            ),
        );
    }

    #[test]
    fn the_later_of_two_declarations_is_reported_and_the_first_stays() {
        let source = concat!(
            "command hello { run = shell(\"first\"); }\n",
            "command HELLO { run = shell(\"second\"); }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (errors, set.get("hello").map(|command| &command.step)),
            (
                vec![fault(
                    name_span(source, "HELLO"),
                    "command `HELLO` is declared twice"
                )],
                Some(&Step::Shell(Template::parse("first"))),
            ),
        );
    }

    #[test]
    fn a_user_command_replaces_the_embedded_one_of_its_name() {
        let shipped = "command hello { run = shell(\"shipped\"); }";
        let mine = "command hello { run = shell(\"mine\"); }";
        let (set, errors) =
            CommandSet::build((&config(shipped), shipped), Some((&config(mine), mine)));
        assert_eq!(
            (
                names(&set),
                set.get("hello").map(|command| &command.step),
                errors
            ),
            (
                vec!["hello"],
                Some(&Step::Shell(Template::parse("mine"))),
                Vec::new()
            ),
        );
    }

    #[test]
    fn a_statement_set_twice_and_a_block_with_no_run_are_reported() {
        let source = concat!(
            "command a { doc = \"x\"; doc = \"y\"; run = shell(\"a\"); run = shell(\"b\"); ",
            "output = notify; output = block; }\n",
            "command bee { doc = \"x\"; }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                Vec::new(),
                vec![
                    fault(span(source, "doc = \"y\";"), "`doc` is set twice"),
                    fault(span(source, "run = shell(\"b\");"), "`run` is set twice"),
                    fault(span(source, "output = block;"), "`output` is set twice"),
                    fault(name_span(source, "bee"), "command `bee` has no run"),
                ],
            ),
        );
    }

    #[test]
    fn an_unknown_kind_output_and_form_are_reported() {
        let source = concat!(
            "command a { arg x = bogus; run = shell(\"a {x}\"); output = loud; }\n",
            "command bee { arg y = text(1); run = shell(\"b {y}\"); }\n",
            "command c { arg z = same_as(\"x\"); run = shell(\"c {z}\"); }\n",
            "command job = other(\"y\");",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                Vec::new(),
                vec![
                    fault(span(source, "bogus"), "unknown argument kind `bogus`"),
                    fault(span(source, "loud"), "unknown output `loud`"),
                    fault(span(source, "text(1)"), "`text` takes no argument"),
                    fault(
                        span(source, "same_as(\"x\")"),
                        "`same_as` takes one command name"
                    ),
                    fault(span(source, "other"), "`other` is not a command form"),
                ],
            ),
        );
    }

    #[test]
    fn a_run_takes_one_shell_or_action_step() {
        let source = concat!(
            "command a { run = [shell(\"a\"), shell(\"b\")]; }\n",
            "command bee { run = shell; }\n",
            "command c { run = shell(); }",
        );
        let bare = span(source, "shell;").start;
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                Vec::new(),
                vec![
                    fault(
                        span(source, "run = [shell(\"a\"), shell(\"b\")];"),
                        "`run` takes one step"
                    ),
                    fault(bare..bare + "shell".len(), "`shell` is not a step"),
                    fault(span(source, "shell()"), "`shell` takes one string"),
                ],
            ),
        );
    }

    /// A placeholder names an argument, and `{{name}}` and `${NAME}` name
    /// none.
    #[test]
    fn a_placeholder_names_an_argument() {
        let source = concat!(
            "command job { run = shell(\"echo {nope}\"); }\n",
            "command fine { run = shell(\"echo {{nope}} ${HOME}\"); }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                vec!["fine"],
                vec![fault(
                    span(source, "shell(\"echo {nope}\")"),
                    "placeholder `{nope}` names no argument of `job`"
                )],
            ),
        );
    }

    #[test]
    fn an_action_step_checks_as_a_binding_does() {
        let source = concat!(
            "command a { run = Nope(); }\n",
            "command bee { run = SetMode(); }\n",
            "command c { arg x = text; run = Notify(git.last_subject); }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                Vec::new(),
                vec![
                    fault(span(source, "Nope()"), "unknown action `Nope`"),
                    fault(span(source, "SetMode()"), "SetMode names no mode"),
                    fault(
                        span(source, "git.last_subject"),
                        "an action argument takes no context value"
                    ),
                ],
            ),
        );
    }

    #[test]
    fn an_action_step_takes_only_the_parameters_that_its_action_declares() {
        let source = concat!(
            "command a { arg what = text; run = OpenRun(\"x\"); }\n",
            "command bee { arg what = text; run = Notify(\"a\", \"b\"); }\n",
            "command c { arg what = text; run = OpenFile(nope: \"x\"); }\n",
            "command d { arg what = text; run = Review(against: \"main\"); }\n",
            "command e { arg what = text; run = Notify(text: \"{what}\"); }\n",
            "command f { arg what = text; run = OpenFile(\"{what}\"); }\n",
            "command g { arg what = text; run = Review(from: \"{what}\", against: \"main\"); }",
        );
        let open_run = span(source, "OpenRun(\"x\")").start + "OpenRun(".len();
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                vec!["e", "f", "g"],
                vec![
                    fault(
                        open_run..open_run + "\"x\"".len(),
                        "action `OpenRun` takes no argument"
                    ),
                    fault(span(source, "\"b\""), "action `Notify` takes 1 argument"),
                    fault(
                        span(source, "nope"),
                        "action `OpenFile` declares no parameter `nope`"
                    ),
                    fault(
                        span(source, "against"),
                        "argument `against` of action `Review` is not at its parameter's place"
                    ),
                ],
            ),
        );
    }

    #[test]
    fn same_as_names_a_command_with_an_argument_and_forms_no_cycle() {
        let source = concat!(
            "command base { arg a = text; run = shell(\"x {a}\"); }\n",
            "command fine { arg b = same_as(base); run = shell(\"x {b}\"); }\n",
            "command lost { arg c = same_as(nothing); run = shell(\"x {c}\"); }\n",
            "command deep { arg g = same_as(lost); run = shell(\"x {g}\"); }\n",
            "command bare { run = shell(\"x\"); }\n",
            "command empty { arg d = same_as(bare); run = shell(\"x {d}\"); }\n",
            "command one { arg e = same_as(two); run = shell(\"x {e}\"); }\n",
            "command two { arg f = same_as(one); run = shell(\"x {f}\"); }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                vec!["base", "fine", "bare"],
                vec![
                    fault(
                        span(source, "same_as(nothing)"),
                        "`same_as(nothing)` names no command"
                    ),
                    fault(
                        span(source, "same_as(bare)"),
                        "command `bare` has no argument"
                    ),
                    fault(span(source, "same_as(two)"), "`same_as(two)` forms a cycle"),
                    fault(span(source, "same_as(one)"), "`same_as(one)` forms a cycle"),
                    fault(
                        span(source, "same_as(lost)"),
                        "`same_as(lost)` names no command"
                    ),
                ],
            ),
        );
    }

    /// `file` and `dir` load bare or with empty parentheses. Each takes no
    /// argument.
    #[test]
    fn file_and_dir_are_argument_kinds() {
        let source = concat!(
            "command show { arg a = file; arg b = dir; arg c = file(); arg d = dir(); ",
            "run = shell(\"x {a} {b} {c} {d}\"); }\n",
            "command bad { arg e = file(1); arg f = dir(x); run = shell(\"x {e} {f}\"); }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), kinds(&set, "show"), errors),
            (
                vec!["show"],
                Some(vec![
                    ArgKind::File,
                    ArgKind::Dir,
                    ArgKind::File,
                    ArgKind::Dir
                ]),
                vec![
                    fault(span(source, "file(1)"), "`file` takes no argument"),
                    fault(span(source, "dir(x)"), "`dir` takes no argument"),
                ],
            ),
        );
    }

    /// A `same_as` argument takes the kind of its target's first argument, at
    /// the end of a chain of two as well.
    #[test]
    fn same_as_takes_the_kind_of_the_named_command_s_first_argument() {
        let source = concat!(
            "command show { arg path = file; run = shell(\"cat {path}\"); }\n",
            "command mirror { arg a = same_as(show); arg b = dir; run = shell(\"x {a} {b}\"); }\n",
            "command echoes { arg c = same_as(mirror); run = shell(\"x {c}\"); }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (kinds(&set, "mirror"), kinds(&set, "echoes"), errors),
            (
                Some(vec![ArgKind::File, ArgKind::Dir]),
                Some(vec![ArgKind::File]),
                Vec::new(),
            ),
        );
    }

    /// A string, a number, and a boolean default to their literal text. The
    /// four context names default to their context values.
    #[test]
    fn a_text_default_is_a_literal_or_a_context_value() {
        let source = concat!(
            "command ctxs { arg a = text(default: \"hi\"); arg b = text(default: 2.50); ",
            "arg c = text(default: true); arg d = text(default: git.last_subject); ",
            "arg e = text(default: selection); arg f = text(default: file); ",
            "arg g = text(default: line); arg h = text(); ",
            "run = shell(\"x {a} {b} {c} {d} {e} {f} {g} {h}\"); }",
        );
        let (set, errors) = load(source);
        let literal = |text: &str| ArgKind::Text {
            default: Some(TextDefault::Literal(text.to_owned())),
        };
        let context = |value| ArgKind::Text {
            default: Some(TextDefault::Context(value)),
        };
        assert_eq!(
            (kinds(&set, "ctxs"), errors),
            (
                Some(vec![
                    literal("hi"),
                    literal("2.50"),
                    literal("true"),
                    context(ContextValue::GitLastSubject),
                    context(ContextValue::Selection),
                    context(ContextValue::File),
                    context(ContextValue::Line),
                    ArgKind::Text { default: None },
                ]),
                Vec::new(),
            ),
        );
    }

    /// An unknown context value is reported at the value. A name other than
    /// `default`, and a second `default`, are reported as well.
    #[test]
    fn an_unknown_context_value_is_reported() {
        let source = concat!(
            "command a { arg x = text(default: git.head); run = shell(\"a {x}\"); }\n",
            "command bee { arg y = text(default: nope); run = shell(\"b {y}\"); }\n",
            "command c { arg z = text(other: 1); run = shell(\"c {z}\"); }\n",
            "command d { arg w = text(default: \"1\", default: \"2\"); run = shell(\"d {w}\"); }",
        );
        let (set, errors) = load(source);
        assert_eq!(
            (names(&set), errors),
            (
                Vec::new(),
                vec![
                    fault(span(source, "git.head"), "unknown context value `git.head`"),
                    fault(span(source, "nope"), "unknown context value `nope`"),
                    fault(span(source, "other"), "`text` takes no argument `other`"),
                    fault(span(source, "default: \"2\""), "`default` is set twice"),
                ],
            ),
        );
    }

    #[test]
    fn an_argument_with_a_default_reads_in_brackets() {
        let (set, errors) = load(concat!(
            "command ship { arg to = text; arg message = text(default: git.last_subject); ",
            "run = shell(\"x {to} {message}\"); }",
        ));
        assert_eq!(
            (set.get("ship").map(|command| command.signature()), errors),
            (Some("<to> [message]".to_owned()), Vec::new()),
        );
    }

    #[test]
    fn the_shipped_config_declares_no_command() {
        let (set, errors) = CommandSet::build((app::embedded_config(), app::DEFAULT_KEYMAP), None);
        assert_eq!((names(&set), errors), (Vec::<&str>::new(), Vec::new()));
    }
}
