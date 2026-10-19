//! The `stoat agent` helpers for an agent that drives this editor.

use clap::Subcommand;
use snafu::{ResultExt, Whatever};
use stoat::{claude_plugin, host::LocalFs};

/// Helpers for an agent that drives this editor.
#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// Write the Claude Code plugin that holds the `stoat:api` skill to the
    /// data directory and print the directory, for
    /// `claude --plugin-dir (stoat agent plugin)`.
    Plugin,
}

pub fn run(sub: AgentCommand) -> Result<(), Whatever> {
    match sub {
        AgentCommand::Plugin => {
            let dir = claude_plugin::plugin_dir().whatever_context("resolve the data directory")?;
            claude_plugin::write(&LocalFs, &dir)
                .with_whatever_context(|_| format!("write the plugin to {}", dir.display()))?;
            println!("{}", dir.display());
            Ok(())
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::default::Args;
    use clap::CommandFactory;
    use stoat::claude_plugin::API_SKILL;

    /// A verb the CLI gains without a section in the skill is a verb no agent
    /// learns, so the skill names every one.
    #[test]
    fn the_skill_documents_every_api_verb() {
        let command = Args::command();
        let api = command
            .find_subcommand("api")
            .expect("the api command exists");
        let missing: Vec<&str> = api
            .get_subcommands()
            .map(|verb| verb.get_name())
            .filter(|name| !matches!(*name, "hook" | "help"))
            .filter(|name| !API_SKILL.contains(&format!("stoat api {name}")))
            .collect();
        assert_eq!(
            missing,
            Vec::<&str>::new(),
            "api verbs the skill does not teach"
        );
    }
}
