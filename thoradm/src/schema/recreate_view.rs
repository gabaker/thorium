//! `thoradm schema apply --recreate-view <name>`: converges the schema, then drops each named
//! declared view and creates it again from the registry, waiting for its build
//!
//! A name that isn't a declared view is refused before anything is read or changed. The admin
//! confirms by typing each view's name on a terminal, or with `--yes` up front, which is required
//! without one. The run exits like `schema apply` and writes the same termination summary.

use std::io::IsTerminal;
use thorium::models::schema::{self, ApplyError, ApplyReport, ViewDef, ViewError};
use tracing::{info, warn};

use super::apply::Failure;
use crate::Error;
use crate::args::{ApplySchema, Args};

/// Convert a view error into thoradm's error, whose exit code an operator Job acts on
///
/// # Arguments
///
/// * `err` - The view error
pub(super) fn view_error(err: ViewError) -> Error {
    match err {
        // a failed statement may succeed when retried, and a view left missing by a failed create
        // is created again by the retried apply
        ViewError::Apply(ApplyError::Query { .. }) | ViewError::NotCreated { .. } => {
            Error::Transient(err.to_string())
        }
        // work this build can't do exits like any unavailable subcommand
        ViewError::Apply(ApplyError::Unavailable { .. }) => Error::NotImplemented(err.to_string()),
        // token ranges that can't be checked are the caller's to fix
        ViewError::BadRanges { .. } => Error::Usage(err.to_string()),
        // an undeclared view or a missing base table is a refusal a retry would repeat
        other => Error::Generic(other.to_string()),
    }
}

/// Confirm recreating views: `--yes` confirms up front; otherwise each view's name is typed on a
/// terminal, and without a terminal the recreate is refused
///
/// # Arguments
///
/// * `views` - The views to recreate
/// * `yes` - Whether `--yes` was given
/// * `terminal` - Whether a terminal is attached to answer a prompt
/// * `ask` - Shows a prompt on the terminal and returns what was typed
fn confirm(
    views: &[ViewDef],
    yes: bool,
    terminal: bool,
    ask: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<(), Error> {
    // say what each recreate costs before anything else
    for view in views {
        warn!("{}", schema::recreate_warning(view));
    }
    // --yes confirms without asking
    if yes {
        warn!("Recreating views confirmed by --yes");
        return Ok(());
    }
    // a prompt needs a terminal to answer it
    if !terminal {
        return Err(Error::Generic(
            "refusing to recreate views: no terminal is attached to confirm it; pass --yes to \
             recreate them without asking"
                .to_owned(),
        ));
    }
    // each view's name is typed to confirm dropping it
    for view in views {
        let prompt = format!(
            "Type the view's name ({}) to drop it and create it again",
            view.name
        );
        let typed = ask(&prompt).map_err(Error::Generic)?;
        if typed.trim() != view.name {
            return Err(Error::Generic(format!(
                "view {} was not recreated: {:?} was typed, not its name",
                view.name,
                typed.trim()
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

/// Apply the declared schema, then recreate the named views, returning what the run did
///
/// # Arguments
///
/// * `cmd` - How to apply the schema, naming the views to recreate
/// * `args` - The arguments thoradm was started with
pub(super) async fn run(cmd: &ApplySchema, args: &Args) -> Result<ApplyReport, Failure> {
    // refuse any name that isn't a declared view before anything is read
    let views = schema::resolve_views(&cmd.recreate_view).map_err(view_error)?;
    // a real run is confirmed before anything changes
    if !cmd.dry_run {
        let terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        confirm(&views, cmd.yes, terminal, &mut ask_terminal)?;
    }
    // converge first, so every base table exists and the schema is recorded
    let mut report = super::apply::run(cmd, args).await?;
    // a view this run just created is already fresh
    let views = views
        .into_iter()
        .filter(|view| !report.created.iter().any(|name| name == view.name))
        .collect::<Vec<_>>();
    if views.is_empty() {
        return Ok(report);
    }
    // recreate the rest on a session of its own, reporting the builds' progress
    let conf = super::load_conf(args)?;
    let session = crate::shared::scylla::get_client(&conf)
        .await
        .map_err(|err| Error::Transient(format!("Scylla can't be reached: {err}")))?;
    let recreated = schema::recreate_views(&session, &conf, &views, cmd.dry_run, &|line| {
        info!("{line}");
    })
    .await
    .map_err(view_error)?;
    // add what the recreate did to the apply's report
    report.statements.extend(recreated.statements);
    if !recreated.dry_run {
        for view in &recreated.recreated {
            info!("Recreated view {view} from the registry and waited for its build");
        }
        report.views_built.extend(recreated.recreated);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use thorium::models::schema::{ApplyError, ViewError, resolve_views};

    use super::{confirm, view_error};

    /// `--yes` confirms without asking, even with no terminal
    #[test]
    fn yes_confirms_without_asking() {
        // a prompt would fail the test
        let views = resolve_views(&["samples"]).expect("declared");
        let mut ask = |_: &str| -> Result<String, String> { panic!("asked with --yes") };
        assert!(confirm(&views, true, false, &mut ask).is_ok());
        assert!(confirm(&views, true, true, &mut ask).is_ok());
    }

    /// Without a terminal and without `--yes` the recreate is refused, exiting 1
    #[test]
    fn no_terminal_needs_yes() {
        // nothing is asked
        let views = resolve_views(&["samples"]).expect("declared");
        let mut ask = |_: &str| -> Result<String, String> { panic!("asked without a terminal") };
        let err = confirm(&views, false, false, &mut ask).expect_err("refused");
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("--yes"), "{err}");
    }

    /// On a terminal each view's name must be typed; a wrong name refuses the whole recreate
    #[test]
    fn each_view_name_is_typed_on_a_terminal() {
        // typing each name confirms, and every prompt names its view
        let views = resolve_views(&["samples", "tags_by_item"]).expect("declared");
        let mut prompts = Vec::new();
        let mut ask = |prompt: &str| -> Result<String, String> {
            prompts.push(prompt.to_owned());
            Ok(if prompt.contains("tags_by_item") {
                " tags_by_item\n".to_owned()
            } else {
                "samples".to_owned()
            })
        };
        assert!(confirm(&views, false, true, &mut ask).is_ok());
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains("samples"), "{prompts:?}");
        // a wrong name refuses
        let mut wrong = |_: &str| -> Result<String, String> { Ok("samples_list".to_owned()) };
        let err = confirm(&views, false, true, &mut wrong).expect_err("refused");
        assert_eq!(err.exit_code(), 1);
    }

    /// Refusals exit 1, a failed statement with the retryable code, and unavailable work 3
    #[test]
    fn view_errors_map_to_the_job_exit_codes() {
        // an undeclared view and a missing base table are refusals
        let undeclared = view_error(ViewError::Undeclared {
            name: "extra".to_owned(),
        });
        assert_eq!(undeclared.exit_code(), 1);
        let missing = view_error(ViewError::MissingBase {
            view: "s3_sha256s".to_owned(),
            base: "s3_ids".to_owned(),
        });
        assert_eq!(missing.exit_code(), 1);
        // a failed statement may succeed on a retry
        let query = view_error(ViewError::Apply(ApplyError::Query {
            statement: "DROP MATERIALIZED VIEW".to_owned(),
            error: "timed out".to_owned(),
        }));
        assert_eq!(query.exit_code(), 4);
        // a create that failed after the drop is retried too, since an apply creates the view
        let not_created = view_error(ViewError::NotCreated {
            view: "samples".to_owned(),
            error: ApplyError::Query {
                statement: "CREATE MATERIALIZED VIEW".to_owned(),
                error: "timed out".to_owned(),
            },
        });
        assert_eq!(not_created.exit_code(), 4);
        assert!(
            not_created.to_string().contains("is now missing"),
            "{not_created}"
        );
        let unavailable = view_error(ViewError::Apply(ApplyError::Unavailable {
            what: "x".to_owned(),
        }));
        assert_eq!(unavailable.exit_code(), 3);
        // ranges that can't be checked are the caller's to fix
        let ranges = view_error(ViewError::BadRanges {
            reason: "a sample of 0 ranges".to_owned(),
        });
        assert_eq!(ranges.exit_code(), 2);
    }
}
