//! Arguments for the shell completions command

use clap_complete::Shell;

/// The text `--help` prints after the arguments, explaining how to install each shell's script
const INSTALL_HELP: &str = "\
The script is generated from Thorctl's own command tree, so it always matches this version of
Thorctl. This command needs no config file or login.

To install the script, write it where your shell looks for completions.

bash:
  mkdir -p ~/.local/share/bash-completion/completions
  thorctl completions bash \\
    > ~/.local/share/bash-completion/completions/thorctl

zsh (any directory on $fpath, named _thorctl):
  mkdir -p ~/.zfunc
  thorctl completions zsh > ~/.zfunc/_thorctl
  Add `fpath=(~/.zfunc $fpath)` to ~/.zshrc before `compinit` runs.

fish:
  mkdir -p ~/.config/fish/completions
  thorctl completions fish > ~/.config/fish/completions/thorctl.fish

PowerShell (adds the script to your profile, named by $PROFILE):
  New-Item -Force -ItemType File $PROFILE
  thorctl completions powershell >> $PROFILE

elvish:
  mkdir -p ~/.config/elvish/lib
  thorctl completions elvish > ~/.config/elvish/lib/thorctl.elv
  Add `use thorctl` to ~/.config/elvish/rc.elv.

Start a new shell afterwards so it loads the script.";

/// Print a shell completion script to stdout
#[derive(clap::Parser, Debug)]
#[clap(after_long_help = INSTALL_HELP)]
pub struct Completions {
    /// The shell to print a completion script for
    #[clap(value_enum)]
    pub shell: Shell,
}
