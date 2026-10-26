//! An instance-scoped function-coverage ledger.
//!
//! Every function-scoped Ghidra tool call a sub-agent makes -- decompiling,
//! disassembling, or querying cross-references and the call graph -- is recorded
//! here so a run can report, entirely statically, how much of the binary was
//! examined. Recording happens in [`crate::thorpal::agents::tool`]'s sub-agent
//! hook, which is the single place that sees both the invoking agent's
//! per-invocation display name and the tool name plus its `target` (and
//! optional `program`) arguments, so no agent has to call any extra tool for
//! this tracking to work.
//!
//! A function counts as *examined* only once its code was actually read, i.e. a
//! tool flagged `examining` in the tool registry (decompile or disassemble) was
//! called on it. The relationship tools are still recorded so the per-function
//! tool breakdown is complete, but they do not on their own mark a function
//! examined. Both flags come from each tool's `#[thorium_derive::thorpal_tool]`
//! attribute via [`crate::thorpal::tools::registry`]. The ledger is
//! held as an `Arc<CoverageLedger>` on the `ThorPal` instance and shared with its
//! sub-agent hooks, so two ThorPal instances keep independent coverage. At the
//! end of a run [`CoverageLedger::log_summary`] emits a per-agent breakdown, an
//! examined/total fraction, and the per-function tool breakdown.
//!
//! Calls against an imported child program (improvement 0011) are recorded
//! under a structural `(program, target)` key -- never a joined string, so a
//! demangled `Namespace::function` name can't masquerade as a child key --
//! and reported separately: the examined/total fraction covers the main
//! program only, since no per-child denominator is tracked.

use std::collections::{BTreeMap, BTreeSet};

use tokio::sync::Mutex;
use tracing::instrument;

use crate::thorpal::tools::registry;
use crate::thorpal::usage::with_thousands;

/// A recorded function's identity: the program scope plus the normalized target.
///
/// The scope is the empty string for the main program and the child program's
/// name otherwise, so the same `FUN_<hex>` in two programs never collides.
type FunctionKey = (String, String);

/// An instance-scoped ledger of per-agent function coverage.
///
/// Keyed by agent display name, then by [`FunctionKey`], to the set of tool
/// names called on that function by that agent. `BTreeMap`/`BTreeSet` keep
/// every level in a stable, sorted order so the summary prints
/// deterministically regardless of the order calls happened to arrive in.
pub struct CoverageLedger {
    /// The per-agent, per-function tool tally, guarded for concurrent recording.
    inner: Mutex<BTreeMap<String, BTreeMap<FunctionKey, BTreeSet<String>>>>,
    /// The main program's name, so a `program` argument naming it explicitly
    /// still counts as main-program coverage.
    main_program: String,
}

impl CoverageLedger {
    /// Creates a new, empty coverage ledger
    ///
    /// # Arguments
    ///
    /// * `main_program` - The main program's (domain file) name
    pub fn new(main_program: String) -> Self {
        CoverageLedger {
            inner: Mutex::new(BTreeMap::new()),
            main_program,
        }
    }

    /// Records a function-scoped tool call against the agent that made it
    ///
    /// Repeated calls accumulate: the tool is added to the set of tools already
    /// seen for that agent and function, so a function touched several times still
    /// counts once per agent while remembering every tool used on it. A call
    /// whose `program` argument is unset, empty, or the main program's own name
    /// records under the main scope; anything else records under that child
    /// program's scope.
    ///
    /// # Arguments
    ///
    /// * `agent` - The per-invocation display name of the calling sub-agent
    /// * `tool` - The name of the function-scoped tool that was called
    /// * `target` - The `target` argument naming the function the tool acted on
    /// * `program` - The call's `program` argument, when one was passed
    #[instrument(skip_all)]
    pub async fn record(&self, agent: &str, tool: &str, target: &str, program: Option<&str>) {
        // resolve the program scope: main collapses onto the empty scope so an
        // explicit main-program arg never leaks into the child bucket
        let scope = program
            .map(str::trim)
            .filter(|name| !name.is_empty() && *name != self.main_program)
            .unwrap_or("")
            .to_owned();
        // normalize the target so equivalent references land on one key
        let key = (scope, normalize_target(target));
        // insert the tool into this agent's set of tools for this function, creating
        // the agent and function entries the first time either is seen
        let mut guard = self.inner.lock().await;
        guard
            .entry(agent.to_owned())
            .or_default()
            .entry(key)
            .or_default()
            .insert(tool.to_owned());
    }

    /// Logs the final function coverage: a per-agent breakdown, an examined/total
    /// fraction, and a per-function tool breakdown
    ///
    /// Intended to be called once, after the run finishes but before Ghidra is
    /// shut down, so the operator can see how much of the binary the agents
    /// examined and which tools touched which function. The examined count is the
    /// number of distinct main-program functions any agent decompiled or
    /// disassembled; `total_functions` is the main program's function count, so
    /// child-program activity is reported separately without a denominator.
    ///
    /// # Arguments
    ///
    /// * `total_functions` - The total number of functions in the main program
    #[instrument(skip_all)]
    pub async fn log_summary(&self, total_functions: usize) {
        // take a snapshot of the ledger so we do not hold the lock across the logs
        let guard = self.inner.lock().await;
        // print a header so the summary stands out among the run's other log lines
        tracing::info!("──────────── FUNCTION COVERAGE ────────────");
        // emit one line per agent: how many main-program functions it examined
        // and touched, with its child-program touches broken out so the agent
        // lines reconcile with the main-only total below
        for (agent, functions) in guard.iter() {
            let children = functions
                .keys()
                .filter(|(scope, _)| !scope.is_empty())
                .count();
            tracing::info!(
                agent = %agent,
                examined = %with_thousands(examined_count(functions, false) as u64),
                touched = %with_thousands((functions.len() - children) as u64),
                child_touched = %with_thousands(children as u64),
                "agent function coverage"
            );
        }
        // merge every agent's coverage so totals and the breakdown dedupe functions
        // that more than one agent touched
        let merged = merge_functions(&guard);
        // the examined total is the count of distinct main-program functions read
        let examined = examined_count(&merged, false);
        // note child-program activity separately when any exists: children have
        // no tracked denominator, so they stay out of the fraction below
        let child_touched = merged.keys().filter(|(scope, _)| !scope.is_empty()).count();
        if child_touched > 0 {
            tracing::info!(
                touched = %with_thousands(child_touched as u64),
                examined = %with_thousands(examined_count(&merged, true) as u64),
                "child-program functions (no denominator; children are unmeasured)"
            );
        }
        // compute the examined fraction, guarding against a zero denominator so an
        // empty or failed function listing does not divide by zero
        let percent = if total_functions == 0 {
            0.0
        } else {
            (examined as f64 / total_functions as f64) * 100.0
        };
        // emit the grand total: examined out of total, with the percentage
        tracing::info!(
            examined = %with_thousands(examined as u64),
            total = %with_thousands(total_functions as u64),
            percent = %format!("{percent:.1}%"),
            "TOTAL function coverage"
        );
        // emit the per-function tool breakdown so it is clear which tools were
        // called on which function across the whole run
        for ((scope, target), tools) in merged.iter() {
            // render the sorted tool list as a compact comma-separated string
            let tools = tools.iter().cloned().collect::<Vec<_>>().join(", ");
            // render child keys with their program prefix (display only)
            if scope.is_empty() {
                tracing::info!(function = %target, tools = %tools, "function tools");
            } else {
                tracing::info!(function = %format!("{scope}::{target}"), tools = %tools, "function tools");
            }
        }
    }
}

/// Normalizes a function `target` so equivalent references dedupe
///
/// Trims surrounding whitespace and strips a single leading `0x` prefix, so that
/// `0x00402574` and `00402574` collapse to the same key. Everything else is kept
/// verbatim.
///
/// Note: a function referenced once by name (e.g. `entry`) and once by address
/// still counts as two distinct targets here. Resolving those to a single
/// canonical identity would require a Ghidra round-trip, which we deliberately
/// avoid so that this tracking stays entirely static and free.
///
/// # Arguments
///
/// * `target` - The raw `target` argument passed to a function-scoped tool
fn normalize_target(target: &str) -> String {
    // drop any surrounding whitespace the model may have included
    let trimmed = target.trim();
    // collapse a hex address's optional `0x` prefix onto its bare form
    let stripped = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    stripped.to_owned()
}

/// Returns whether a set of tools used on a function counts as examining it
///
/// A function is examined once any registry tool flagged `examining`
/// (decompile or disassemble) has been called on it; relationship tools alone
/// do not count.
///
/// # Arguments
///
/// * `tools` - The set of tool names called on a single function
fn is_examined(tools: &BTreeSet<String>) -> bool {
    // examined if the tool set includes at least one examining tool
    tools.iter().any(|tool| registry::is_examining(tool))
}

/// Collects the merged, cross-agent map of every function to the tools used on it
///
/// Folds every agent's per-function tool sets together so a function touched by
/// more than one agent lists the union of all tools used on it. This is the
/// basis for both the total examined count and the per-function tool breakdown.
///
/// # Arguments
///
/// * `ledger` - A snapshot of the coverage ledger to merge
fn merge_functions(
    ledger: &BTreeMap<String, BTreeMap<FunctionKey, BTreeSet<String>>>,
) -> BTreeMap<FunctionKey, BTreeSet<String>> {
    // start from an empty merged map and fold every agent's coverage into it
    let mut merged: BTreeMap<FunctionKey, BTreeSet<String>> = BTreeMap::new();
    // walk each agent, then each function that agent touched
    for functions in ledger.values() {
        for (key, tools) in functions {
            // union this agent's tools for the function into the merged entry
            merged
                .entry(key.clone())
                .or_default()
                .extend(tools.iter().cloned());
        }
    }
    merged
}

/// Counts how many functions in one scope of a per-function map were examined
///
/// # Arguments
///
/// * `functions` - A map of function key to the set of tools used on it
/// * `children` - Count the child-program scope instead of the main one
fn examined_count(functions: &BTreeMap<FunctionKey, BTreeSet<String>>, children: bool) -> usize {
    // count only the requested scope's functions whose tool set examined them
    functions
        .iter()
        .filter(|((scope, _), _)| scope.is_empty() != children)
        .filter(|(_, tools)| is_examined(tools))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `0x` hex prefix and surrounding whitespace are normalized away.
    #[test]
    fn normalize_target_strips_prefix_and_trims() {
        // a `0x`-prefixed address collapses onto its bare hex form
        assert_eq!(normalize_target("0x00402574"), "00402574");
        // surrounding whitespace is trimmed before stripping
        assert_eq!(normalize_target("  0x00402574 "), "00402574");
        // a bare name is kept verbatim
        assert_eq!(normalize_target("entry"), "entry");
        // a demangled C++ name keeps its namespace separators verbatim
        assert_eq!(normalize_target("Crypto::decrypt"), "Crypto::decrypt");
    }

    /// Recording scopes keys by program, collapsing the main program onto main.
    #[tokio::test]
    async fn record_scopes_programs_structurally() {
        let ledger = CoverageLedger::new("main.exe".to_owned());
        // a bare call, an explicit main-program call, and a child call
        ledger
            .record("RE(Io)", "decomp_function", "0x401000", None)
            .await;
        ledger
            .record("RE(Io)", "decomp_function", "401000", Some("main.exe"))
            .await;
        ledger
            .record("RE(Io)", "decomp_function", "0x401000", Some("payload.exe"))
            .await;
        let guard = ledger.inner.lock().await;
        let functions = guard.get("RE(Io)").expect("agent should be recorded");
        // the bare and explicit-main calls collapsed onto one main-scope key
        assert_eq!(functions.len(), 2);
        assert!(functions.contains_key(&(String::new(), "401000".to_owned())));
        // the child call kept its own scope, so addresses never collide
        assert!(functions.contains_key(&("payload.exe".to_owned(), "401000".to_owned())));
    }

    /// Only the examining tools mark a function's tool set as examined.
    #[test]
    fn is_examined_requires_an_examining_tool() {
        // a function only cross-referenced was not examined
        let xrefs_only = BTreeSet::from(["xrefs_to".to_owned()]);
        assert!(!is_examined(&xrefs_only));
        // a decompiled function was examined
        let decompiled = BTreeSet::from(["decomp_function".to_owned()]);
        assert!(is_examined(&decompiled));
        // a disassembled function was examined even alongside relationship tools
        let mixed = BTreeSet::from(["disassemble_function".to_owned(), "callers".to_owned()]);
        assert!(is_examined(&mixed));
    }

    /// Merging unions tools per function; scoped counting splits main from child.
    #[test]
    fn merge_and_examined_count_split_scopes() {
        // two agents each touch main `entry`; one also decompiles a child fn
        let mut ledger: BTreeMap<String, BTreeMap<FunctionKey, BTreeSet<String>>> = BTreeMap::new();
        ledger
            .entry("ReverseEngineer(Io)".to_owned())
            .or_default()
            .insert(
                (String::new(), "entry".to_owned()),
                BTreeSet::from(["decomp_function".to_owned()]),
            );
        ledger
            .entry("ReverseEngineer(Europa)".to_owned())
            .or_default()
            .insert(
                (String::new(), "entry".to_owned()),
                BTreeSet::from(["callers".to_owned()]),
            );
        ledger
            .entry("ReverseEngineer(Europa)".to_owned())
            .or_default()
            .insert(
                ("payload.exe".to_owned(), "entry".to_owned()),
                BTreeSet::from(["decomp_function".to_owned()]),
            );
        let merged = merge_functions(&ledger);
        // the merged main entry unions both agents' tools for `entry`
        assert_eq!(
            merged.get(&(String::new(), "entry".to_owned())),
            Some(&BTreeSet::from([
                "decomp_function".to_owned(),
                "callers".to_owned()
            ]))
        );
        // main and child scopes count separately, never bleeding together
        assert_eq!(examined_count(&merged, false), 1);
        assert_eq!(examined_count(&merged, true), 1);
    }
}
