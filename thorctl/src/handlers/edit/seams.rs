//! The terminal checks and prompt answers behind the edit flow
//!
//! Everything that depends on the environment (is there a terminal, what does a prompt answer)
//! is read once into [`Seams`] so the flow can be driven in tests without touching the process
//! environment. Three hidden environment variables let scripts and tests drive the real binary
//! without a terminal:
//!
//! * `THORCTL_EDIT_ASSUME_TTY=1` treats the session as a terminal for the editor check only.
//! * `THORCTL_EDIT_PROMPT=cancel|reedit` answers the error prompt (and `cancel` declines the
//!   confirmation) when stdin is not a terminal. Without it a failed edit is cancelled and
//!   saved to a recovery file.
//! * `THORCTL_EDIT_BEFORE_APPLY=COMMAND` runs a shell command after the resource is read and
//!   before the remote-change check, so a test can change the resource in between.

use std::io::IsTerminal;
use thorium::Error;

/// The environment variable that treats the session as a terminal
pub const ASSUME_TTY_VAR: &str = "THORCTL_EDIT_ASSUME_TTY";

/// The environment variable that answers prompts when stdin is not a terminal
pub const PROMPT_VAR: &str = "THORCTL_EDIT_PROMPT";

/// The environment variable that names a command to run before the remote-change check
pub const BEFORE_APPLY_VAR: &str = "THORCTL_EDIT_BEFORE_APPLY";

/// The answer given to prompts when there is no terminal to ask
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswer {
    /// Abandon the edit (and decline the confirmation)
    Cancel,
    /// Reopen the editor after an error
    Reedit,
}

/// The environment the edit flow runs in
#[derive(Debug, Clone, Default)]
pub struct Seams {
    /// Whether stdin and stderr are both a terminal, so prompts can be drawn
    pub terminal: bool,
    /// Whether the session is treated as a terminal for the editor check only
    pub assume_tty: bool,
    /// The answer to prompts when there is no terminal
    pub prompt: Option<PromptAnswer>,
    /// A shell command to run before the remote-change check
    pub before_apply: Option<String>,
}

impl Seams {
    /// Read the seams from the process: the real terminal state and the hidden variables
    #[must_use]
    pub fn from_env() -> Self {
        // an assumed terminal needs the variable set to something other than 0 or empty
        let assume_tty =
            std::env::var(ASSUME_TTY_VAR).is_ok_and(|value| !value.is_empty() && value != "0");
        // only the two known answers are honored
        let prompt = match std::env::var(PROMPT_VAR).as_deref() {
            Ok("cancel") => Some(PromptAnswer::Cancel),
            Ok("reedit") => Some(PromptAnswer::Reedit),
            _ => None,
        };
        // the hook is a plain shell command
        let before_apply = std::env::var(BEFORE_APPLY_VAR)
            .ok()
            .filter(|value| !value.is_empty());
        Seams {
            terminal: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
            assume_tty,
            prompt,
            before_apply,
        }
    }

    /// Whether an editor can be opened (a real terminal, or one assumed for tests)
    #[must_use]
    pub fn can_edit(&self) -> bool {
        self.terminal || self.assume_tty
    }

    /// Ask whether to apply the changes just shown
    ///
    /// # Arguments
    ///
    /// * `question` - The question to put to the user
    pub fn confirm(&self, question: &str) -> Result<bool, Error> {
        // ask on a real terminal
        if self.terminal {
            return dialoguer::Confirm::new()
                .with_prompt(question)
                .default(true)
                .interact()
                .map_err(|err| Error::new(format!("Failed to read user input: {err}")));
        }
        // otherwise only an explicit cancel declines
        Ok(self.prompt != Some(PromptAnswer::Cancel))
    }

    /// Ask whether to reopen the editor after an error
    ///
    /// # Arguments
    ///
    /// * `choices` - The menu entries, with the entry that reopens the editor first
    pub fn reedit(&self, choices: &[&str]) -> Result<bool, Error> {
        // ask on a real terminal
        if self.terminal {
            let selection = dialoguer::Select::new()
                .items(choices)
                .default(0)
                .interact()
                .map_err(|err| Error::new(format!("Failed to read user input: {err}")))?;
            return Ok(selection == 0);
        }
        // otherwise only an explicit reedit reopens the editor
        Ok(self.prompt == Some(PromptAnswer::Reedit))
    }

    /// Run the before-apply hook, if one is set
    pub fn run_before_apply(&self) -> Result<(), Error> {
        // nothing to run without a hook
        let Some(command) = &self.before_apply else {
            return Ok(());
        };
        // run the command through the platform shell
        let status = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", command])
                .status()
        } else {
            std::process::Command::new("sh")
                .args(["-c", command])
                .status()
        }
        .map_err(|err| Error::new(format!("Failed to run {BEFORE_APPLY_VAR}: {err}")))?;
        // a failing hook aborts the edit
        if status.success() {
            Ok(())
        } else {
            Err(Error::new(format!(
                "{BEFORE_APPLY_VAR} exited with {status}"
            )))
        }
    }
}
