//! `thoradm schema drop-legacy`: drops registered legacy objects, the one exception to never
//! dropping an object the registry doesn't declare
//!
//! A run is a dry run unless `--execute` is given: it prints the plan, each object's row count and
//! what a downgrade does to it afterwards, and drops nothing. With `--execute` the admin types
//! each object's id on a terminal, or passes `--confirm` up front (required without a terminal).
//! Every object is named with `--object`; there is no default of all. A declared object, an id
//! nothing registers, and an object a revision in the support window needs are refused with exit
//! code 1 before anything is dropped. Exit codes: 0 done or a dry run, 1 a refusal, 2 a config that
//! can't be used, 3 Scylla unavailable, 4 a read or statement that failed and may succeed again.

use std::fmt::Write;
use std::io::IsTerminal;
use thorium::models::schema::{
    DropPlan, LegacySources, drop_planned, inspect, plan_drops, resolve_legacy,
};
use tracing::info;

use super::legacy::{connect, legacy_error};
use crate::Error;
use crate::args::{Args, DropLegacy};
use crate::outcome::RunOutcome;

/// Say who is running the command: the OS user and host, as `user@host`
fn who() -> String {
    // the login name and host from the environment, `unknown` where the shell sets neither
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|host| !host.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|host| host.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    format!("{user}@{host}")
}

/// Render a plan as the text a dry run and a real run print before anything is dropped
///
/// # Arguments
///
/// * `plan` - The drops
/// * `execute` - Whether the run drops the objects
#[must_use]
pub fn render(plan: &DropPlan, execute: bool) -> String {
    // say whether this run changes anything
    let mut text = if execute {
        String::from("Plan\n")
    } else {
        String::from("Plan (dry run: nothing is dropped; pass --execute to drop)\n")
    };
    for drop in &plan.drops {
        // the object with its kind and rows, or that there is nothing to drop
        if drop.present {
            let _ = writeln!(
                text,
                "  {}  ({}, {} rows)",
                drop.id,
                drop.kind.label(),
                drop.rows
            );
            let _ = writeln!(text, "    statement: {}", drop.statement);
        } else {
            let _ = writeln!(
                text,
                "  {}  ({}, not present: nothing to drop)",
                drop.id,
                drop.kind.label()
            );
        }
        // what a downgrade does to it afterwards
        for target in &drop.downgrades {
            let _ = writeln!(
                text,
                "    downgrade to {}: {}",
                target.revision,
                target.outcome.describe(drop.kind)
            );
        }
    }
    text
}

/// Confirm dropping: `--confirm` confirms up front; otherwise each present object's id is typed on a
/// terminal, and without a terminal the drop is refused
///
/// # Arguments
///
/// * `plan` - The drops
/// * `confirm` - Whether `--confirm` was given
/// * `terminal` - Whether a terminal is attached to answer a prompt
/// * `ask` - Shows a prompt on the terminal and returns what was typed
fn confirm(
    plan: &DropPlan,
    confirm: bool,
    terminal: bool,
    ask: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<(), Error> {
    // --confirm confirms without asking
    if confirm {
        info!("Dropping legacy objects confirmed by --confirm");
        return Ok(());
    }
    // a prompt needs a terminal to answer it
    if !terminal {
        return Err(Error::Generic(
            "refusing to drop legacy objects: no terminal is attached to confirm it; without a \
             terminal `--execute` needs `--confirm`"
                .to_owned(),
        ));
    }
    // each object's id is typed to confirm dropping it; an absent object has nothing to confirm
    for drop in plan.drops.iter().filter(|drop| drop.present) {
        let prompt = format!(
            "Type the object's id ({}) to drop it and its {} rows",
            drop.id, drop.rows
        );
        let typed = ask(&prompt).map_err(Error::Generic)?;
        if typed.trim() != drop.id {
            return Err(Error::Generic(format!(
                "nothing was dropped: {:?} was typed, not {}",
                typed.trim(),
                drop.id
            )));
        }
    }
    Ok(())
}

/// Ask on the terminal and return what was typed
///
/// # Arguments
///
/// * `prompt` - What to ask
fn ask_terminal(prompt: &str) -> Result<String, String> {
    // read one line of text, which may be empty
    dialoguer::Input::<String>::new()
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()
        .map_err(|err| format!("failed to ask for confirmation: {err}"))
}

/// Drop the legacy objects the admin named, or print what would be dropped
///
/// # Arguments
///
/// * `cmd` - The objects to drop and whether to execute
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &DropLegacy, args: &Args) -> Result<RunOutcome, Error> {
    // refuse a declared object or an id nothing registers before anything is read
    let sources = LegacySources::production();
    let entries =
        resolve_legacy(&cmd.object, &sources).map_err(|err| Error::Generic(err.to_string()))?;
    // read what the keyspace holds, and plan against it
    let conf = super::load_conf(args)?;
    let session = connect(&conf).await?;
    let listing = inspect(&session, &conf, &sources)
        .await
        .map_err(|err| legacy_error(&err))?;
    let plan = plan_drops(&conf.thorium.namespace, &entries, &listing)
        .map_err(|err| Error::Generic(err.to_string()))?;
    print!("{}", render(&plan, cmd.execute));
    // a dry run stops after saying what it would do
    if !cmd.execute {
        return Ok(RunOutcome::Complete);
    }
    // a real run is confirmed before anything is dropped
    let terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    confirm(&plan, cmd.confirm, terminal, &mut ask_terminal)?;
    // drop each object, recording who did it
    let records = drop_planned(&session, &conf, &plan, &who(), &|line| println!("{line}"))
        .await
        .map_err(|err| legacy_error(&err))?;
    println!("dropped {} legacy object(s)", records.len());
    Ok(RunOutcome::Complete)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use thorium::models::schema::{LegacySources, classify, plan_drops, resolve_legacy};

    use super::{confirm, render};

    /// Plan dropping both exemplars from a keyspace holding the table with rows and not the view
    fn plan() -> thorium::models::schema::DropPlan {
        // the table is live with rows, the view is absent
        let sources = LegacySources::production();
        let live = BTreeMap::from([("events".to_owned(), "table".to_owned())]);
        let rows = BTreeMap::from([("events".to_owned(), 18204)]);
        let listing = classify(&sources, &live, &rows);
        let ids = ["scylla:events", "scylla:entities_by_name"].map(ToOwned::to_owned);
        let entries = resolve_legacy(&ids, &sources).expect("registered");
        plan_drops("ks", &entries, &listing).expect("droppable")
    }

    /// A dry run says nothing is dropped and how to drop; a real run's plan doesn't
    #[test]
    fn the_plan_names_each_object_and_its_downgrade_consequence() {
        let text = render(&plan(), false);
        let expected = "Plan (dry run: nothing is dropped; pass --execute to drop)
  scylla:events  (table, 18204 rows)
    statement: DROP TABLE IF EXISTS ks.events
    downgrade to 2026-10-v03: recreated (it does not use this object)
    downgrade to 2026-10-v02: recreated (it does not use this object)
    downgrade to 2026-10-v01: recreated (it does not use this object)
  scylla:entities_by_name  (view, not present: nothing to drop)
    downgrade to 2026-10-v03: recreated (it does not use this object)
    downgrade to 2026-10-v02: recreated (it does not use this object)
    downgrade to 2026-10-v01: recreated (it does not use this object)
";
        assert_eq!(text, expected);
        assert!(render(&plan(), true).starts_with("Plan\n"));
    }

    /// `--confirm` confirms without asking, even with no terminal
    #[test]
    fn confirm_flag_skips_the_prompts() {
        // a prompt would fail the test
        let mut ask = |_: &str| -> Result<String, String> { panic!("asked with --confirm") };
        assert!(confirm(&plan(), true, false, &mut ask).is_ok());
        assert!(confirm(&plan(), true, true, &mut ask).is_ok());
    }

    /// Without a terminal and without `--confirm` the drop is refused, exiting 1
    #[test]
    fn no_terminal_needs_confirm() {
        // nothing is asked
        let mut ask = |_: &str| -> Result<String, String> { panic!("asked without a terminal") };
        let err = confirm(&plan(), false, false, &mut ask).expect_err("refused");
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("--confirm"), "{err}");
    }

    /// On a terminal each present object's id must be typed; a wrong answer aborts, and an
    /// absent object isn't asked about
    #[test]
    fn each_object_id_is_typed_on_a_terminal() {
        // only the present table is prompted, by its id
        let mut prompts = Vec::new();
        let mut ask = |prompt: &str| -> Result<String, String> {
            prompts.push(prompt.to_owned());
            Ok(" scylla:events\n".to_owned())
        };
        assert!(confirm(&plan(), false, true, &mut ask).is_ok());
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("scylla:events"), "{prompts:?}");
        assert!(prompts[0].contains("18204"), "{prompts:?}");
        // a wrong answer refuses the whole drop
        let mut wrong = |_: &str| -> Result<String, String> { Ok("events".to_owned()) };
        let err = confirm(&plan(), false, true, &mut wrong).expect_err("refused");
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("nothing was dropped"), "{err}");
    }
}
