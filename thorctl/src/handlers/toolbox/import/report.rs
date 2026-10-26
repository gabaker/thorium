//! What a toolbox import did, as a summary people read and a document scripts parse

use serde::{Deserialize, Serialize};

use super::plan::PlanDocument;
use crate::handlers::toolbox::v2::plan_types::{ItemKind, ItemRef, Plan};
use crate::utils::output::{View, table_from};

/// What happened to one item
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// The item was created
    Created,
    /// The item was changed (a tool gained versions, or an item was updated in place)
    Updated,
    /// The item already matched the toolbox
    Unchanged,
    /// The item differs and was left alone
    Skipped,
    /// The item doesn't exist and an update doesn't create it
    NotImported,
    /// Writing the item failed
    Failed,
    /// The item was never reached because an earlier item failed
    NotAttempted,
}

impl Action {
    /// Get the word this action is shown with
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // each action has a fixed word
        match self {
            Action::Created => "created",
            Action::Updated => "updated",
            Action::Unchanged => "unchanged",
            Action::Skipped => "skipped",
            Action::NotImported => "not imported",
            Action::Failed => "failed",
            Action::NotAttempted => "not attempted",
        }
    }
}

/// The result for one item of the toolbox
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemResult {
    /// The item
    pub item: ItemRef,
    /// The group the item is in
    pub group: String,
    /// What happened to it
    pub action: Action,
    /// The details, such as the versions published or the reason for a failure
    pub detail: String,
}

/// How an import ended
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Nothing was written because the import was a dry run
    DryRun,
    /// The toolbox already matched, so there was nothing to do
    NothingToDo,
    /// Every change was applied
    Applied,
    /// Something failed and what was applied was kept
    Failed,
    /// Something failed and what was applied was rolled back
    RolledBack,
    /// Nothing was written because the import was refused up front
    Refused,
    /// Nothing was written because it was declined
    Cancelled,
}

impl Outcome {
    /// Get the sentence this outcome is shown with
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // each outcome has a fixed sentence
        match self {
            Outcome::DryRun => "dry run, nothing was written",
            Outcome::NothingToDo => "nothing to do",
            Outcome::Applied => "applied",
            Outcome::Failed => "failed, what was applied was kept",
            Outcome::RolledBack => "failed, what was applied was rolled back",
            Outcome::Refused => "refused, nothing was written",
            Outcome::Cancelled => "cancelled, nothing was written",
        }
    }
}

/// Everything an import did, which is the document `-o json` prints
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    /// How the import ended
    pub outcome: Outcome,
    /// The plan the import followed
    pub plan: Plan,
    /// The groups that were created
    pub groups_created: Vec<String>,
    /// The network policies that were created
    pub policies_created: Vec<String>,
    /// The network policies that were changed
    pub policies_updated: Vec<String>,
    /// What happened to each item, in the order they were applied
    pub results: Vec<ItemResult>,
    /// What was undone by a rollback, in the order it was undone
    pub rolled_back: Vec<String>,
    /// What could not be undone
    pub rollback_errors: Vec<String>,
    /// What stopped the import
    pub errors: Vec<String>,
    /// Anything else worth knowing
    pub notes: Vec<String>,
}

impl ImportReport {
    /// Start a report for a plan
    ///
    /// # Arguments
    ///
    /// * `plan` - The plan the import follows
    /// * `outcome` - How the import ends unless something changes it
    #[must_use]
    pub fn new(plan: Plan, outcome: Outcome) -> Self {
        // nothing has happened yet
        ImportReport {
            outcome,
            notes: plan.notes.clone(),
            plan,
            groups_created: Vec::new(),
            policies_created: Vec::new(),
            policies_updated: Vec::new(),
            results: Vec::new(),
            rolled_back: Vec::new(),
            rollback_errors: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Count the items with an action
    ///
    /// # Arguments
    ///
    /// * `action` - The action to count
    #[must_use]
    pub fn count(&self, action: Action) -> usize {
        // tally the items that ended that way
        self.results
            .iter()
            .filter(|result| result.action == action)
            .count()
    }

    /// Whether the import succeeded
    #[must_use]
    pub fn success(&self) -> bool {
        // dry runs, no-ops, and applied imports are successes
        matches!(
            self.outcome,
            Outcome::DryRun | Outcome::NothingToDo | Outcome::Applied | Outcome::Cancelled
        )
    }
}

impl View for ImportReport {
    /// Render a row per item
    fn table(&self) -> String {
        // one row per result
        let rows = self
            .results
            .iter()
            .map(|result| {
                vec![
                    result.item.kind.to_string(),
                    result.item.name.clone(),
                    result.group.clone(),
                    result.action.as_str().to_owned(),
                    result.detail.clone(),
                ]
            })
            .collect();
        table_from(&["KIND", "NAME", "GROUP", "RESULT", "DETAIL"], rows)
    }

    /// Render the result as text
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text
    fn human(&self, _ansi: bool) -> String {
        let mut lines = Vec::new();
        // what was created outside the items
        for group in &self.groups_created {
            lines.push(format!("created group {group}"));
        }
        for policy in &self.policies_created {
            lines.push(format!("created network policy {policy}"));
        }
        for policy in &self.policies_updated {
            lines.push(format!("updated network policy {policy}"));
        }
        // each item that was written or could not be
        for result in &self.results {
            if matches!(result.action, Action::Unchanged) {
                continue;
            }
            let detail = if result.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", result.detail)
            };
            lines.push(format!(
                "{} {} {}{detail}",
                result.action.as_str(),
                result.item.kind,
                result.item.name
            ));
        }
        // what a rollback undid
        for line in &self.rolled_back {
            lines.push(format!("rolled back: {line}"));
        }
        for line in &self.rollback_errors {
            lines.push(format!("could not roll back: {line}"));
        }
        for line in &self.errors {
            lines.push(format!("error: {line}"));
        }
        for line in &self.notes {
            lines.push(format!("note: {line}"));
        }
        // the totals
        lines.push(format!(
            "Result: {} ({} created, {} updated, {} unchanged, {} skipped, {} not imported, {} failed)",
            self.outcome.as_str(),
            self.count(Action::Created),
            self.count(Action::Updated),
            self.count(Action::Unchanged),
            self.count(Action::Skipped),
            self.count(Action::NotImported),
            self.count(Action::Failed) + self.count(Action::NotAttempted),
        ));
        lines.join("\n")
    }
}

impl View for PlanDocument {
    /// Render a row per item and what happens to it
    fn table(&self) -> String {
        // one row per item of the plan
        let rows = self
            .plan
            .items
            .iter()
            .map(|item| {
                vec![
                    item.item.kind.to_string(),
                    item.item.name.clone(),
                    item.group.clone(),
                    format!("{:?}", item.state).to_lowercase(),
                    plan_detail(item),
                ]
            })
            .collect();
        table_from(&["KIND", "NAME", "GROUP", "STATE", "DETAIL"], rows)
    }

    /// Render the plan as text
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text
    fn human(&self, _ansi: bool) -> String {
        let plan = &self.plan;
        let mut lines = Vec::new();
        // groups that are missing or can't be written to
        for group in &plan.groups {
            match group.state {
                crate::handlers::toolbox::v2::plan_types::GroupState::Exists => {}
                state => lines.push(format!("group {}: {state:?}", group.name).to_lowercase()),
            }
        }
        // the network policies
        for policy in &plan.policies {
            lines
                .push(format!("network policy {}: {:?}", policy.name, policy.state).to_lowercase());
        }
        // one line per item that isn't already as the toolbox says
        for item in &plan.items {
            let state = format!("{:?}", item.state).to_lowercase();
            if state == "unchanged" {
                continue;
            }
            let detail = plan_detail(item);
            lines.push(format!(
                "{state:<12} {} {} ({}){}",
                item.item.kind,
                item.item.name,
                item.group,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
            if let Some(diff) = &item.diff {
                for line in diff.lines() {
                    lines.push(format!("    {line}"));
                }
            }
        }
        for note in &plan.notes {
            lines.push(format!("note: {note}"));
        }
        for error in &self.errors {
            lines.push(format!("error: {error}"));
        }
        // the totals
        let summary = plan.summary();
        lines.push(format!(
            "Plan: {} to create, {} to update, {} unchanged, {} skipped, {} not imported",
            summary.create,
            summary.update,
            summary.unchanged,
            summary.skipped,
            summary.not_imported
        ));
        lines.join("\n")
    }
}

/// Describe what happens to the versions of a tool, or what a conflict says
///
/// # Arguments
///
/// * `item` - The planned item
fn plan_detail(item: &crate::handlers::toolbox::v2::plan_types::ItemPlan) -> String {
    // a conflict says what differs
    if let Some(conflict) = &item.conflict {
        return conflict.detail.clone();
    }
    // a tool lists the versions that will be published
    if item.item.kind == ItemKind::Tool {
        return item
            .versions
            .iter()
            .map(|version| format!("{} ({:?})", version.label, version.class).to_lowercase())
            .collect::<Vec<String>>()
            .join(", ");
    }
    String::new()
}
