use clap::Parser;
use stoat::cli::config::{Cli, Command};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        // Command::Workspace(workspace_cmd) => {
        //     println!("Workspace command: {:?}", workspace_cmd);
        //     // TODO: Implement workspace command handling
        // },
        // Command::Node(node_cmd) => {
        //     println!("Node command: {:?}", node_cmd);
        //     // TODO: Implement node command handling
        // },
        // Command::Link(link_args) => {
        //     println!("Link command: {:?}", link_args);
        //     // TODO: Implement link command handling
        // },
        // Command::Run(run_args) => {
        //     println!("Run command: {:?}", run_args);
        //     // TODO: Implement run command handling
        // },
        Command::Csv(csv_cmd) => {
            println!("CSV command: {:?}", csv_cmd);
            // TODO: Implement CSV command handling
        },
        // Command::Status(status_args) => {
        //     println!("Status command: {:?}", status_args);
        //     // TODO: Implement status command handling
        // },
        // Command::Repl => {
        //     println!("Starting REPL mode...");
        //     // TODO: Implement REPL mode
        // },
    }
}
