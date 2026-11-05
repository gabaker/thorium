//! `thoradm migrate report`: prints what a migration's last pass applied
//!
//! The `drain-reactions` and `resubmit-reactions` reports are read from the `drain_request` rows
//! instead: every request a drain cancel recorded, with its outcome, as tables or as JSON. Neither
//! reads the `drain_request_args` rows or prints a request's arguments, their hash, or their size.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use thorium::models::ReactionStatus;
use thorium::models::schema::DrainCancelReport;
use thorium::models::schema::drain_requests::{DrainRequestRow, DrainRequestStore, DrainStatus};

use super::context::{self, Env, migrate_error};
use crate::Error;
use crate::args::{Args, ReportMigration};
use crate::outcome::RunOutcome;

/// The id of the drain cancel, whose report lists the requests recorded and the workers removed
const DRAIN_ID: &str = super::drain::ID;

/// The id of the resubmission, whose report lists each request's outcome
const RESUBMIT_ID: &str = "resubmit-reactions";

/// How many request rows one read of the report takes
const PAGE_ROWS: usize = 500;

/// What a cell shows for a value the row doesn't hold
const NONE: &str = "-";

/// What a cell shows for a value of a request whose stored JSON can't be read
const UNREADABLE: &str = "?";

/// One recorded request as the reports show it: who and what it ran and how far its resubmission
/// got, never its arguments
#[derive(Debug, Clone, Serialize)]
struct RequestLine {
    /// The cancelled reaction's id
    id: String,
    /// How far its resubmission got
    status: DrainStatus,
    /// The reaction's group, unless its stored request can't be read
    group: Option<String>,
    /// The reaction's pipeline, unless its stored request can't be read
    pipeline: Option<String>,
    /// The user who created the reaction, unless its stored request can't be read
    creator: Option<String>,
    /// The reaction's status when it was cancelled, unless its stored request can't be read
    old_status: Option<ReactionStatus>,
    /// The reaction's parent, if it was a sub reaction
    parent: Option<String>,
    /// The drain attempt that recorded it, if its run named one
    attempt: Option<String>,
    /// The new reaction id reserved for it
    entry: Option<String>,
    /// The new reaction id once resubmitted, or the reason once refused or stale
    result: Option<String>,
    /// When the reaction was cancelled
    recorded: Option<DateTime<Utc>>,
    /// When the request was resubmitted, refused, or marked stale
    finished: Option<DateTime<Utc>>,
    /// Whether the stored request couldn't be read, so its group, pipeline, and creator are unknown
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    unreadable: bool,
}

impl RequestLine {
    /// Build the line of a stored request row, keeping only what the reports show
    ///
    /// # Arguments
    ///
    /// * `row` - The stored row
    fn of(row: DrainRequestRow) -> Self {
        // the request's fields, when its JSON can be read
        let request = row.request().ok();
        let unreadable = request.is_none();
        let (group, pipeline, creator, old_status, parent) = match request {
            Some(request) => (
                Some(request.group),
                Some(request.pipeline),
                Some(request.creator),
                Some(request.old_status),
                request.parent.map(|parent| parent.to_string()),
            ),
            None => (None, None, None, None, None),
        };
        RequestLine {
            id: row.id,
            status: row.status,
            group,
            pipeline,
            creator,
            old_status,
            parent,
            attempt: row.attempt,
            entry: row.entry,
            result: row.result,
            recorded: row.started,
            finished: row.finished,
            unreadable,
        }
    }

    /// What the resubmission's last column shows: the new id, the reserved id, or the reason
    fn outcome(&self) -> String {
        // each status names what it ended with
        match self.status {
            DrainStatus::Resubmitted => {
                let id = self.result.as_ref().or(self.entry.as_ref());
                let id = id.map_or(NONE, String::as_str);
                // a sub reaction is resubmitted as a root, without its parent's ephemeral files
                if self.parent.is_some() {
                    format!("{id} (resubmitted as a root, without its parent's ephemeral files)")
                } else {
                    id.to_owned()
                }
            }
            DrainStatus::Resubmitting => match &self.entry {
                Some(entry) => format!("reserved {entry}"),
                None => NONE.to_owned(),
            },
            DrainStatus::Refused | DrainStatus::Stale => {
                self.result.clone().unwrap_or_else(|| NONE.to_owned())
            }
            DrainStatus::Recorded => NONE.to_owned(),
        }
    }
}

/// A drain report as JSON: the requests and their counts by status, and for the cancel its
/// stored report
#[derive(Debug, Serialize)]
struct DrainRequestReport {
    /// The migration the report is for
    migration: &'static str,
    /// How many requests hold each status
    counts: BTreeMap<&'static str, u64>,
    /// Every recorded request, in id order
    requests: Vec<RequestLine>,
    /// What the last finished cancel removed, in the drain-reactions report only, and only when
    /// a cancel pass finished with its report
    #[serde(skip_serializing_if = "Option::is_none")]
    cancel: Option<DrainCancelReport>,
}

/// Count the requests by status, every status listed
///
/// # Arguments
///
/// * `requests` - The requests to count
fn counts(requests: &[RequestLine]) -> BTreeMap<&'static str, u64> {
    // start every status at zero so the counts always name all of them
    let mut counts = DrainStatus::ALL
        .iter()
        .map(|status| (status.as_str(), 0))
        .collect::<BTreeMap<_, _>>();
    for request in requests {
        *counts.entry(request.status.as_str()).or_default() += 1;
    }
    counts
}

/// The count summary line under a request table, statuses in their declared order
///
/// # Arguments
///
/// * `requests` - The requests to sum up
fn summary(requests: &[RequestLine]) -> String {
    // the nonzero statuses in the order they are declared
    let counts = counts(requests);
    let parts = DrainStatus::ALL
        .iter()
        .filter_map(|status| {
            let count = counts.get(status.as_str()).copied().unwrap_or_default();
            (count > 0).then(|| format!("{count} {status}"))
        })
        .collect::<Vec<_>>();
    format!("{} request(s): {}\n", requests.len(), parts.join(", "))
}

/// Make a stored value safe for one table cell: no control characters, never empty
///
/// # Arguments
///
/// * `value` - The value
fn cell(value: Option<&str>) -> String {
    // control characters (newlines among them) would break the table's lines
    match value {
        Some(value) if !value.is_empty() => value
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
        _ => NONE.to_owned(),
    }
}

/// A request's group, pipeline, and creator cells, marked unknown when its request can't be read
///
/// # Arguments
///
/// * `request` - The request
fn who(request: &RequestLine) -> [String; 3] {
    // an unreadable request shows a question mark rather than a blank
    let known = |value: &Option<String>| {
        if request.unreadable {
            UNREADABLE.to_owned()
        } else {
            cell(value.as_deref())
        }
    };
    [
        known(&request.group),
        known(&request.pipeline),
        known(&request.creator),
    ]
}

/// Format a timestamp in UTC without fractions
///
/// # Arguments
///
/// * `time` - The timestamp, if there is one
fn stamp(time: Option<DateTime<Utc>>) -> String {
    // RFC 3339 to the second
    time.map_or_else(
        || NONE.to_owned(),
        |time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// Lay rows out as a table whose columns are as wide as their widest cell
///
/// # Arguments
///
/// * `header` - The column titles
/// * `rows` - The cells of each row
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    // each column is as wide as its widest cell, and columns are two spaces apart
    let mut widths: Vec<usize> = header.iter().map(|title| title.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    // a line pads each cell to its column's width
    let line = |cells: Vec<&str>| {
        let padded = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>();
        format!("{}\n", padded.join("  ").trim_end())
    };
    // the header, then each row
    let mut text = line(header.to_vec());
    for row in rows {
        text.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    text
}

/// Render the resubmission's report: each request's outcome and its new id or reason
///
/// # Arguments
///
/// * `requests` - Every recorded request, in id order
fn render_resubmit(requests: &[RequestLine]) -> String {
    // with nothing recorded there is nothing to tabulate
    if requests.is_empty() {
        return "No drain requests are recorded\n".to_owned();
    }
    // one row per request, then the counts
    let rows = requests
        .iter()
        .map(|request| {
            let [group, pipeline, creator] = who(request);
            vec![
                request.id.clone(),
                group,
                pipeline,
                creator,
                request.status.to_string(),
                cell(Some(&request.outcome())),
            ]
        })
        .collect::<Vec<_>>();
    let header = [
        "OLD ID",
        "GROUP",
        "PIPELINE",
        "CREATOR",
        "OUTCOME",
        "NEW ID / REASON",
    ];
    let mut text = table(&header, &rows);
    text.push_str(&summary(requests));
    text
}

/// Render the cancel's report: each request recorded, then the worker registrations removed
///
/// # Arguments
///
/// * `requests` - Every recorded request, in id order
/// * `cancel` - What the last finished cancel pass stored, if one finished with a report
fn render_drain(requests: &[RequestLine], cancel: Option<&DrainCancelReport>) -> String {
    // the requests recorded, or a line saying there are none
    let mut text = if requests.is_empty() {
        "No drain requests are recorded\n".to_owned()
    } else {
        let rows = requests
            .iter()
            .map(|request| {
                let [group, pipeline, creator] = who(request);
                let was = request
                    .old_status
                    .as_ref()
                    .map_or_else(|| UNREADABLE.to_owned(), ToString::to_string);
                vec![
                    request.id.clone(),
                    group,
                    pipeline,
                    creator,
                    was,
                    stamp(request.recorded),
                    cell(request.attempt.as_deref()),
                    request.status.to_string(),
                ]
            })
            .collect::<Vec<_>>();
        let header = [
            "ID", "GROUP", "PIPELINE", "CREATOR", "WAS", "RECORDED", "ATTEMPT", "STATUS",
        ];
        let mut text = String::from("Requests recorded\n");
        text.push_str(&table(&header, &rows));
        text.push_str(&summary(requests));
        text
    };
    // the last cancel's counts and the registrations it removed, from its stored report
    let Some(cancel) = cancel else {
        let _ = writeln!(
            text,
            "\n{DRAIN_ID} has no finished cancel with a stored report, so no removed worker \
             registrations are listed"
        );
        return text;
    };
    let _ = writeln!(
        text,
        "\nThe last cancel failed {} reaction(s), recorded {} request(s) to resubmit, {} refused, \
         and {} stale, kept {} an earlier run recorded, and removed {} worker registration(s)",
        cancel.cancelled,
        cancel.recorded,
        cancel.refused,
        cancel.stale,
        cancel.kept,
        cancel.workers_removed
    );
    // a table of the removed registrations it listed
    if !cancel.workers.is_empty() {
        let rows = cancel
            .workers
            .iter()
            .map(|worker| {
                vec![
                    cell(Some(&worker.name)),
                    cell(Some(&worker.group)),
                    cell(Some(&worker.scaler)),
                    cell(Some(&worker.cluster)),
                    cell(Some(&worker.node)),
                ]
            })
            .collect::<Vec<_>>();
        text.push_str("\nWorker registrations removed\n");
        text.push_str(&table(
            &["NAME", "GROUP", "SCALER", "CLUSTER", "NODE"],
            &rows,
        ));
    }
    // registrations past the listed cap are only counted
    if cancel.workers_not_listed > 0 {
        let _ = writeln!(
            text,
            "... and {} more removed registration(s) not listed",
            cancel.workers_not_listed
        );
    }
    text
}

/// Read every recorded request a page at a time, in id order
///
/// # Arguments
///
/// * `env` - The connected backends
async fn read_requests(env: &Env) -> Result<Vec<RequestLine>, Error> {
    // only the request rows are read, never their arguments rows
    let store = DrainRequestStore::prepare(env.backends.scylla.clone(), &env.backends.namespace)
        .await
        .map_err(migrate_error)?;
    let mut requests = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .list_page(after.as_deref(), PAGE_ROWS)
            .await
            .map_err(migrate_error)?;
        let full = page.len() == PAGE_ROWS;
        after = page.last().map(|row| row.id.clone());
        requests.extend(page.into_iter().map(RequestLine::of));
        // a short page is the last one
        if !full || after.is_none() {
            return Ok(requests);
        }
    }
}

/// Print a drain report, as tables or as JSON
///
/// # Arguments
///
/// * `cmd` - The report to print
/// * `env` - The connected backends
async fn drain_report(cmd: &ReportMigration, env: &Env) -> Result<RunOutcome, Error> {
    // the requests, and for the cancel the report its last finished pass stored
    let requests = read_requests(env).await?;
    let (migration, cancel) = if cmd.id == DRAIN_ID {
        let pass = super::find(DRAIN_ID)?.pass;
        let row = env.store.pass_row(pass).await.map_err(migrate_error)?;
        let cancel = row
            .and_then(|row| row.report)
            .and_then(|report| report.drain);
        (DRAIN_ID, cancel)
    } else {
        (RESUBMIT_ID, None)
    };
    // JSON when asked, tables otherwise
    if cmd.json {
        let report = DrainRequestReport {
            migration,
            counts: counts(&requests),
            requests,
            cancel,
        };
        println!("{}", serde_json::to_string(&report)?);
    } else if migration == DRAIN_ID {
        print!("{}", render_drain(&requests, cancel.as_ref()));
    } else {
        print!("{}", render_resubmit(&requests));
    }
    Ok(RunOutcome::Complete)
}

/// Print the applied report stored in a migration's pass row, or for the drain and the
/// resubmission the requests the drain recorded
///
/// # Arguments
///
/// * `cmd` - The migration whose report to print
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &ReportMigration, args: &Args) -> Result<RunOutcome, Error> {
    // the drain and the resubmission report the recorded requests
    if cmd.id == DRAIN_ID || cmd.id == RESUBMIT_ID {
        let env = context::connect(args).await?;
        return drain_report(cmd, &env).await;
    }
    // read the pass row's report, which only a pass that ended has
    let def = super::find(&cmd.id)?;
    let env = context::connect(args).await?;
    let row = env.store.pass_row(def.pass).await.map_err(migrate_error)?;
    match row.and_then(|row| row.report) {
        Some(report) => {
            println!("{}", serde_json::to_string(&report)?);
            Ok(RunOutcome::Complete)
        }
        None => Err(Error::new(format!(
            "{} has no applied report; its last pass hasn't ended",
            cmd.id
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thorium::models::schema::DrainWorker;
    use thorium::models::schema::drain_requests::{
        CreatorMembership, DrainRequest, PipelineIdentity, args_sha256,
    };

    /// The arguments the test rows hold, which no rendering may print
    const ARGS: &str = "{\"positionals\":[\"UNIT-ARGS-CANARY\"]}";

    /// A stored row for a request in group corn
    ///
    /// # Arguments
    ///
    /// * `id` - The cancelled reaction's id
    /// * `status` - How far its resubmission got
    /// * `entry` - The reserved new id
    /// * `result` - The new id or the reason
    /// * `parent` - The reaction's parent, if it was a sub reaction
    fn row(
        id: &str,
        status: DrainStatus,
        entry: Option<&str>,
        result: Option<&str>,
        parent: Option<&str>,
    ) -> DrainRequestRow {
        // a request whose arguments are only hashed and counted
        let request = DrainRequest {
            group: "corn".to_owned(),
            pipeline: "unpack".to_owned(),
            samples: vec!["sha".to_owned()],
            repos: Vec::new(),
            tags: Vec::new(),
            parent: parent.map(|parent| parent.parse().unwrap()),
            trigger_depth: None,
            creator: "alice".to_owned(),
            old_status: ReactionStatus::Created,
            last_activity: None,
            orphan: false,
            args_sha256: Some(args_sha256(ARGS)),
            args_bytes: ARGS.len() as u64,
            buffers: false,
            cache: false,
            identity: PipelineIdentity {
                group: "corn".to_owned(),
                name: "unpack".to_owned(),
                creator: "alice".to_owned(),
                order_sha256: "0".repeat(64),
            },
            membership: CreatorMembership {
                held_role: true,
                role: "user".to_owned(),
            },
            binding: None,
        };
        DrainRequestRow {
            id: id.to_owned(),
            value: request.to_json().unwrap(),
            attempt: Some("2026-11-04T10:05:00Z".to_owned()),
            status,
            entry: entry.map(str::to_owned),
            result: result.map(str::to_owned),
            revision: None,
            started: None,
            finished: None,
            updated: None,
        }
    }

    /// The lines every rendering test reads: one request in each status, a sub reaction, and an
    /// unreadable row
    fn lines() -> Vec<RequestLine> {
        // the rows as the store returns them
        let parent = "0b6d1c3e-6f2a-4c55-9a51-2f4c3e8d1a77";
        let mut unreadable = row("u", DrainStatus::Recorded, None, None, None);
        unreadable.value = "not json".to_owned();
        [
            row(
                "a",
                DrainStatus::Resubmitted,
                Some("new-a"),
                Some("new-a"),
                None,
            ),
            row(
                "b",
                DrainStatus::Refused,
                None,
                Some("line one\nline two"),
                None,
            ),
            row("c", DrainStatus::Stale, None, Some("idle"), None),
            row("d", DrainStatus::Recorded, None, None, None),
            row("e", DrainStatus::Resubmitting, Some("new-e"), None, None),
            row(
                "f",
                DrainStatus::Resubmitted,
                Some("new-f"),
                Some("new-f"),
                Some(parent),
            ),
            unreadable,
        ]
        .into_iter()
        .map(RequestLine::of)
        .collect()
    }

    /// The resubmission's table names each outcome with its new id, reserved id, or reason, keeps
    /// every row on one line, and never prints the arguments or their hash
    #[test]
    fn the_resubmit_table_shows_outcomes_and_no_args() {
        // render every status
        let text = render_resubmit(&lines());
        let row_of = |id: &str| {
            text.lines()
                .find(|line| line.starts_with(&format!("{id} ")))
                .unwrap_or_else(|| panic!("{id}: {text}"))
                .to_owned()
        };
        assert!(text.starts_with("OLD ID"), "{text}");
        assert!(row_of("a").contains(" resubmitted "), "{text}");
        assert!(row_of("a").ends_with(" new-a"), "{text}");
        assert!(row_of("b").contains("line one line two"), "{text}");
        assert!(row_of("e").contains("reserved new-e"), "{text}");
        assert!(row_of("f").contains("as a root"), "{text}");
        assert!(row_of("u").contains('?'), "{text}");
        assert!(text.contains("7 request(s): 2 recorded, 1 resubmitting, 2 resubmitted"));
        assert!(!text.contains("UNIT-ARGS-CANARY") && !text.contains(&args_sha256(ARGS)));
    }

    /// The drain's report lists the requests and the removed workers, says how many weren't
    /// listed, and says so when no cancel has finished
    #[test]
    fn the_drain_table_lists_requests_and_removed_workers() {
        // a cancel that removed one listed worker and two unlisted ones
        let cancel = DrainCancelReport {
            cancelled: 7,
            workers_removed: 3,
            workers_not_listed: 2,
            workers: vec![DrainWorker {
                group: "corn".to_owned(),
                name: "corn-unpack-1".to_owned(),
                scaler: "K8s".to_owned(),
                cluster: "prod".to_owned(),
                node: "node-3".to_owned(),
            }],
            ..DrainCancelReport::default()
        };
        let text = render_drain(&lines(), Some(&cancel));
        assert!(text.contains("Requests recorded\nID"), "{text}");
        assert!(text.contains("failed 7 reaction(s)"), "{text}");
        assert!(
            text.contains("Worker registrations removed\nNAME"),
            "{text}"
        );
        assert!(text.contains("corn-unpack-1  corn   K8s"), "{text}");
        assert!(text.contains("2 more removed registration(s)"), "{text}");
        assert!(!text.contains("UNIT-ARGS-CANARY") && !text.contains(&args_sha256(ARGS)));
        // with no finished cancel the report says so
        let text = render_drain(&[], None);
        assert!(text.starts_with("No drain requests are recorded"), "{text}");
        assert!(text.contains("no finished cancel"), "{text}");
    }

    /// The JSON report names every status's count and holds no arguments or hash
    #[test]
    fn the_json_report_counts_every_status_and_holds_no_args() {
        // serialize the resubmission's report
        let requests = lines();
        let report = DrainRequestReport {
            migration: RESUBMIT_ID,
            counts: counts(&requests),
            requests,
            cancel: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("UNIT-ARGS-CANARY") && !json.contains(&args_sha256(ARGS)));
        assert!(!json.contains("args"), "{json}");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["counts"]["refused"], 1);
        assert_eq!(value["counts"]["stale"], 1);
        assert_eq!(value["requests"][6]["unreadable"], true);
        assert!(value["requests"][0].get("unreadable").is_none());
        assert!(value.get("cancel").is_none());
    }
}
