//! Summarize a batch run's unit statuses, attempts, usage, and cost

use std::path::Path;

use tabled::settings::Style;
use tabled::{Table, Tabled};
use thorium::{CtlConf, Error};

use super::super::pricing::CostModel;
use super::super::session::to_thorpal_provider;
use super::scan::{self, UsageLine};
use super::state::{self, RunState, UnitStatus};
use crate::args::Args;
use crate::args::ai::BatchStatus;

/// One row of the status table
#[derive(Tabled)]
struct StatusRow {
    /// The unit's manifest name
    #[tabled(rename = "Unit")]
    unit: String,
    /// The unit's blind alias
    #[tabled(rename = "Alias")]
    alias: String,
    /// The unit's session name
    #[tabled(rename = "Session")]
    session: String,
    /// Where the unit stands
    #[tabled(rename = "Status")]
    status: String,
    /// How many attempts the unit has had
    #[tabled(rename = "Attempts")]
    attempts: usize,
    /// Whether the unit's report file exists with content
    #[tabled(rename = "Report")]
    report: String,
    /// The unit's total token consumption
    #[tabled(rename = "Tokens")]
    tokens: String,
    /// The unit's estimated cost in dollars
    #[tabled(rename = "Cost")]
    cost: String,
}

/// Print a status table for a batch run
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The status command to run
pub async fn handle(args: &Args, cmd: &BatchStatus) -> Result<(), Error> {
    // resolve the evaluation directory the run lives under
    let dir = std::fs::canonicalize(&cmd.dir).map_err(|err| {
        Error::new(format!(
            "failed to resolve evaluation dir {}: {err}",
            cmd.dir.display()
        ))
    })?;
    // pick the requested run or fall back to the latest one
    let run = match cmd.run {
        Some(run) => run,
        None => state::latest_run(&dir)?
            .ok_or_else(|| Error::new(format!("{} has no runs yet", dir.display())))?,
    };
    let run_dir = state::run_dir(&dir, run);
    // a run without state has nothing to summarize
    let state = RunState::load(&run_dir)?.ok_or_else(|| {
        Error::new(format!(
            "run {run} has no state under {}",
            run_dir.display()
        ))
    })?;
    // pricing is best effort: without a usable config the cost column thins
    let cost_model = CtlConf::from_path(&args.config).ok().and_then(|conf| {
        let resolved = conf.ai.as_ref().and_then(|ai| ai.resolve().ok())?;
        let overrides = resolved.pricing();
        CostModel::for_provider(&to_thorpal_provider(resolved), overrides.as_ref())
    });
    // build one row per unit plus the run totals
    let mut rows = Vec::with_capacity(state.units.len());
    let mut totals = Totals::default();
    for (name, unit) in &state.units {
        // refresh the scoreability facts live off the run artifacts
        let report = report_nonempty(&run_dir, &unit.session);
        let tail = scan::parse_usage_tail(&run_dir.join(format!("{}.err", unit.session)));
        let usage = unit.usage.or(tail.usage);
        // prefer the recorded cost, then the live tail, then local pricing
        let cost = unit
            .cost
            .or(tail.cost)
            .or_else(|| usage.and_then(|usage| price(&cost_model, &usage)));
        // a running unit whose pid died is stale, not actually running
        let status = match unit.status {
            UnitStatus::Running if !unit.pid.is_some_and(state::pid_alive) => {
                "running (stale)".to_owned()
            }
            status => format!("{status:?}").to_lowercase(),
        };
        totals.add(unit.status, usage.as_ref(), cost);
        rows.push(StatusRow {
            unit: name.clone(),
            alias: unit.alias.clone(),
            session: unit.session.clone(),
            status,
            attempts: unit.attempts.len(),
            report: if report {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
            tokens: usage.map_or("—".to_owned(), |usage| group_thousands(usage.total)),
            cost: cost.map_or("—".to_owned(), |cost| format!("${cost:.2}")),
        });
    }
    // print the table and the totals line
    println!("{}", Table::new(rows).with(Style::psql()));
    println!(
        "run {run}: {} complete, {} failed, {} other; {} tokens, ${:.2} estimated",
        totals.complete,
        totals.failed,
        totals.other,
        group_thousands(totals.tokens),
        totals.cost
    );
    Ok(())
}

/// The accumulated run totals under the status table
#[derive(Default)]
struct Totals {
    /// How many units completed
    complete: usize,
    /// How many units failed
    failed: usize,
    /// How many units are in any other status
    other: usize,
    /// The total tokens consumed across units
    tokens: u64,
    /// The total estimated dollars across units
    cost: f64,
}

impl Totals {
    /// Fold one unit into the totals
    ///
    /// # Arguments
    ///
    /// * `status` - The unit's status
    /// * `usage` - The unit's usage, when known
    /// * `cost` - The unit's estimated cost, when known
    fn add(&mut self, status: UnitStatus, usage: Option<&UsageLine>, cost: Option<f64>) {
        // bucket the status counts
        match status {
            UnitStatus::Complete => self.complete += 1,
            UnitStatus::Failed => self.failed += 1,
            _ => self.other += 1,
        }
        // fold in the known usage and cost
        if let Some(usage) = usage {
            self.tokens += usage.total;
        }
        if let Some(cost) = cost {
            self.cost += cost;
        }
    }
}

/// Check whether a session's report file exists with content
///
/// # Arguments
///
/// * `run_dir` - The run directory holding the reports
/// * `session` - The session the report belongs to
fn report_nonempty(run_dir: &Path, session: &str) -> bool {
    std::fs::metadata(run_dir.join(format!("{session}.md"))).is_ok_and(|meta| meta.len() > 0)
}

/// Price a usage line with the local cost model, when one resolved
///
/// # Arguments
///
/// * `model` - The cost model, when the provider's model is priceable
/// * `usage` - The usage line to price
fn price(model: &Option<CostModel>, usage: &UsageLine) -> Option<f64> {
    // without a model the cost stays unknown
    let model = model.as_ref()?;
    // the stderr tail carries the same four buckets the model prices
    Some(model.cost(
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
    ))
}

/// Format a count with comma thousands separators
///
/// # Arguments
///
/// * `value` - The count to format
fn group_thousands(value: u64) -> String {
    // the figures module owns the shared digit-grouping helper
    super::figures::group_digits(&value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thousands grouping matches the scoreboard's comma style
    #[test]
    fn thousands_grouping() {
        // small counts stay bare
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        // larger counts group in threes from the right
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(83_839_528), "83,839,528");
        assert_eq!(group_thousands(1_000_000_007), "1,000,000,007");
    }
}
