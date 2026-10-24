//! `thoradm schema verify-views`: compares declared views with their base tables
//!
//! The check is read-only. It prints its report as JSON and exits 0 when every view checked is in
//! step, 1 when some aren't (naming them), 2 when it refuses arguments that parsed (such as an
//! undeclared view or a sample of 0 ranges) or can't use the config, and 4 when Scylla can't be
//! reached or a read fails. Arguments that don't parse, such as `--sample abc`, exit 1 like every
//! thoradm command.

use thorium::models::schema::{self, RangeSelection, TOKEN_RANGES, VerifyOptions};
use tracing::{error, info};

use crate::Error;
use crate::args::{Args, VerifyViews};
use crate::outcome::RunOutcome;

/// Turn the arguments into the check's options, refusing an undeclared view or a sample of no
/// ranges or of more ranges than there are
///
/// # Arguments
///
/// * `cmd` - Which views to check and how many ranges
fn options(cmd: &VerifyViews) -> Result<VerifyOptions, Error> {
    // a named view must be declared
    if let Some(view) = &cmd.view {
        schema::resolve_views(&[view]).map_err(|err| Error::Usage(err.to_string()))?;
    }
    // a sample checks between one range and every range
    let ranges = match cmd.sample {
        None => RangeSelection::All,
        Some(count) if (1..=TOKEN_RANGES).contains(&count) => RangeSelection::Sample(count),
        Some(count) => {
            return Err(Error::Usage(format!(
                "--sample {count} can't be checked; give 1 to {TOKEN_RANGES} ranges"
            )));
        }
    };
    Ok(VerifyOptions {
        view: cmd.view.clone(),
        ranges,
    })
}

/// Compare declared materialized views with their base tables
///
/// # Arguments
///
/// * `cmd` - Which views to check and how many ranges
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &VerifyViews, args: &Args) -> Result<RunOutcome, Error> {
    // refuse unusable arguments before reading the config or connecting
    let options = options(cmd)?;
    let conf = super::load_conf(args)?;
    let session = crate::shared::scylla::get_client(&conf)
        .await
        .map_err(|err| Error::Transient(format!("Scylla can't be reached: {err}")))?;
    // check the views, logging each result as it comes
    let report = schema::verify_views(&session, &conf, &options, &|line| info!("{line}"))
        .await
        .map_err(super::recreate_view::view_error)?;
    println!("{}", serde_json::to_string(&report)?);
    // name the views out of step, which make the check fail
    let out = report
        .out_of_step()
        .iter()
        .map(|check| check.view.as_str())
        .collect::<Vec<_>>();
    if out.is_empty() {
        info!("Every view checked is in step with its base table");
        return Ok(RunOutcome::Complete);
    }
    error!(
        "{} views are out of step with their base tables: {}; repair them with `thoradm schema \
         apply --recreate-view <name>`",
        out.len(),
        out.join(", ")
    );
    Ok(RunOutcome::Aborted)
}

#[cfg(test)]
mod tests {
    use thorium::models::schema::RangeSelection;

    use super::options;
    use crate::args::VerifyViews;

    /// The arguments become the check's options; an undeclared view and an unusable sample are
    /// usage errors
    #[test]
    fn arguments_become_options() {
        // every view in full, and one view sampled
        let all = options(&VerifyViews {
            view: None,
            sample: None,
        })
        .expect("usable");
        assert_eq!(all.ranges, RangeSelection::All);
        let one = options(&VerifyViews {
            view: Some("samples".to_owned()),
            sample: Some(3),
        })
        .expect("usable");
        assert_eq!(one.view.as_deref(), Some("samples"));
        assert_eq!(one.ranges, RangeSelection::Sample(3));
        // refusals exit 2
        for cmd in [
            VerifyViews {
                view: Some("samples_list".to_owned()),
                sample: None,
            },
            VerifyViews {
                view: None,
                sample: Some(0),
            },
            VerifyViews {
                view: None,
                sample: Some(257),
            },
        ] {
            let err = options(&cmd).expect_err("refused");
            assert_eq!(err.exit_code(), 2, "{cmd:?}");
        }
    }
}
