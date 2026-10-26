//! Asking a person the questions an import raises

use colored::Colorize;

use super::Decider;
use crate::handlers::progress::suspend;
use crate::handlers::toolbox::v2::plan_types::{Conflict, Plan, Resolution};

/// A decider that asks on the terminal
///
/// The progress bar is hidden while a question is on screen. Prompts are drawn on stderr, so
/// stdout stays free for the report.
#[derive(Debug, Default)]
pub struct TerminalDecider;

impl TerminalDecider {
    /// Build a decider for the terminal
    ///
    /// # Arguments
    ///
    /// * `_bar` - The progress bar that is hidden while a question is asked
    #[must_use]
    pub fn new(_bar: &crate::handlers::progress::Bar) -> Self {
        // nothing to remember: every question hides the bars itself
        TerminalDecider
    }
}

impl Decider for TerminalDecider {
    /// Ask whether to apply, skip, or stop at an existing item that differs
    ///
    /// # Arguments
    ///
    /// * `conflict` - How the item differs
    /// * `diff` - The difference, when there is one to show
    fn resolve(&mut self, conflict: &Conflict, diff: Option<&str>) -> Resolution {
        suspend(|| {
            // show what differs
            eprintln!(
                "\n{} {} ({}) differs from the toolbox: {}",
                "!".bright_yellow(),
                conflict.item,
                conflict.group,
                conflict.detail
            );
            if let Some(diff) = diff {
                for line in diff.lines() {
                    eprintln!("    {line}");
                }
            }
            // ask what to do about it
            let choices = ["Apply the toolbox's version", "Skip it", "Quit"];
            let picked = dialoguer::Select::new()
                .with_prompt("What should the import do?")
                .items(&choices)
                .default(1)
                .interact();
            // an unanswerable prompt stops the import before anything more is written
            match picked {
                Ok(0) => Resolution::Apply,
                Ok(1) => Resolution::Skip,
                _ => Resolution::Quit,
            }
        })
    }

    /// Ask whether to write the plan
    ///
    /// # Arguments
    ///
    /// * `plan` - The plan
    fn confirm(&mut self, plan: &Plan) -> bool {
        suspend(|| {
            // summarize what is about to be written
            let summary = plan.summary();
            let missing: Vec<&str> = plan
                .groups
                .iter()
                .filter(|group| {
                    group.state == crate::handlers::toolbox::v2::plan_types::GroupState::Missing
                })
                .map(|group| group.name.as_str())
                .collect();
            eprintln!(
                "\nThe import will create {} and update {} item(s), create {} network \
                 policy(ies), and create {} group(s).",
                summary.create,
                summary.update,
                plan.policies
                    .iter()
                    .filter(|policy| {
                        policy.state
                            == crate::handlers::toolbox::v2::plan_types::PolicyState::Create
                    })
                    .count(),
                missing.len(),
            );
            dialoguer::Confirm::new()
                .with_prompt("Go ahead?")
                .default(true)
                .interact()
                .unwrap_or(false)
        })
    }

    /// Ask whether to roll back what was applied
    ///
    /// # Arguments
    ///
    /// * `applied` - What was applied, one line per change
    fn rollback(&mut self, applied: &[String]) -> bool {
        suspend(|| {
            // list what would be undone
            eprintln!(
                "\n{} change(s) were applied before the import stopped:",
                applied.len().to_string().bright_yellow()
            );
            for line in applied {
                eprintln!("  {line}");
            }
            dialoguer::Confirm::new()
                .with_prompt("Roll back the changes listed above?")
                .default(false)
                .interact()
                .unwrap_or(false)
        })
    }
}
