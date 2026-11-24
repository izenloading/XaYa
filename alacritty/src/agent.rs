//! The service CLI runs in its own binary, independent of the window event loop.

use std::error::Error;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::cli::AgentOptions;

pub fn forward(options: AgentOptions) -> Result<(), Box<dyn Error>> {
    execute(None, options)
}

pub fn forward_command(command: &str, options: AgentOptions) -> Result<(), Box<dyn Error>> {
    execute(Some(command), options)
}

fn execute(command: Option<&str>, options: AgentOptions) -> Result<(), Box<dyn Error>> {
    let companion = std::env::current_exe()?.with_file_name("xaya-agentd");
    let mut process = Command::new(&companion);
    if let Some(command) = command {
        process.arg(command);
    }
    let error = process.args(options.arguments).exec();
    Err(std::io::Error::new(
        error.kind(),
        format!(
            "could not execute {}: {error}; build or install xaya and xaya-agentd together",
            companion.display(),
        ),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use crate::cli::{Options, Subcommands};
    use clap::Parser;

    #[test]
    fn service_flags_and_help_are_forwarded_verbatim() {
        for verb in
            ["agent", "run", "attach", "checkpoint", "check", "project", "land", "review", "task"]
        {
            for arguments in
                [vec!["--help"], vec!["--runtime-dir", "/tmp/private", "status", "--json"]]
            {
                let options = Options::try_parse_from(
                    ["xaya", verb].into_iter().chain(arguments.iter().copied()),
                )
                .unwrap();
                let agent = match options.subcommands {
                    Some(
                        Subcommands::Agent(agent)
                        | Subcommands::Run(agent)
                        | Subcommands::Attach(agent)
                        | Subcommands::Checkpoint(agent)
                        | Subcommands::Check(agent)
                        | Subcommands::Project(agent)
                        | Subcommands::Land(agent)
                        | Subcommands::Review(agent)
                        | Subcommands::Task(agent),
                    ) => agent,
                    _ => panic!("expected service command"),
                };
                assert_eq!(
                    agent.arguments,
                    arguments.iter().map(std::ffi::OsString::from).collect::<Vec<_>>()
                );
            }
        }
    }
}
