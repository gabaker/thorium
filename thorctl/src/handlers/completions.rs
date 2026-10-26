//! Handles the completions command

use clap::CommandFactory;
use clap_complete::Shell;
use std::io::{ErrorKind, Write};
use thorium::Error;

use crate::args::{Args, completions::Completions};

/// Render the completion script for a shell from Thorctl's real command tree
///
/// # Arguments
///
/// * `shell` - The shell to generate the script for
pub fn script(shell: Shell) -> Vec<u8> {
    // build the real command tree that clap parses arguments with
    let mut command = Args::command();
    // name the script after the binary, as users will type it
    let name = command.get_name().to_owned();
    // generate the script for the shell into memory
    let mut buffer = Vec::new();
    clap_complete::generate(shell, &mut command, name, &mut buffer);
    buffer
}

/// Print a shell completion script to stdout
///
/// # Arguments
///
/// * `cmd` - The completions command to execute
pub fn handle(cmd: &Completions) -> Result<(), Error> {
    // render the script for the requested shell
    let script = script(cmd.shell);
    // lock stdout and write the whole script in one go
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(&script).and_then(|()| stdout.flush()) {
        Ok(()) => Ok(()),
        // a reader like `head` that closes the pipe early is not an error
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(Error::from(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shells Thorctl prints completion scripts for
    const SHELLS: [Shell; 5] = [
        Shell::Bash,
        Shell::Zsh,
        Shell::Fish,
        Shell::PowerShell,
        Shell::Elvish,
    ];

    /// Every shell's script generates, is non-empty, and names thorctl
    #[test]
    fn every_shell_generates_a_script() {
        // generate each shell's script and check it
        for shell in SHELLS {
            let text = String::from_utf8(script(shell)).expect("script was not UTF-8");
            assert!(!text.trim().is_empty(), "{shell} script is empty");
            assert!(
                text.contains("thorctl"),
                "{shell} script never names thorctl"
            );
        }
    }

    /// The bash script lists top level subcommands from the real command tree
    #[test]
    fn bash_script_names_known_subcommands() {
        // generate the bash script
        let text = String::from_utf8(script(Shell::Bash)).expect("script was not UTF-8");
        // each subcommand must appear in it
        for name in ["tools", "workflows", "runs", "files", "completions"] {
            assert!(text.contains(name), "bash script is missing {name}");
        }
    }

    /// The script follows the command tree, so it names every top level subcommand
    #[test]
    fn script_covers_every_top_level_subcommand() {
        // generate the bash script
        let text = String::from_utf8(script(Shell::Bash)).expect("script was not UTF-8");
        // build the command tree the script came from
        let command = Args::command();
        // every top level subcommand must appear in the script
        for sub in command.get_subcommands() {
            assert!(
                text.contains(sub.get_name()),
                "bash script is missing {}",
                sub.get_name()
            );
        }
    }
}
