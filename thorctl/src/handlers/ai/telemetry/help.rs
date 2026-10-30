//! Per-screen help pages for both telemetry TUIs.
//!
//! Every page is static markdown rendered through the shell's
//! [`Popup`](super::shell::Popup), reachable with `?` (or `Space` then `h` in
//! the explorer) on the screen it describes. The column-name constants here
//! also drive the screens' table headers, so a new column cannot land without
//! the `help_covers_every_column` test forcing its explanation onto this page.

/// The fleet table's column names, driving its header and its help
pub const FLEET_COLUMNS: [&str; 6] = ["session", "tokens", "fails", "stalls", "asks", "model"];

/// What each fleet column means, index-aligned with [`FLEET_COLUMNS`]
const FLEET_BLURBS: [&str; 6] = [
    "the session's name, or its id prefix when a bundle shipped no name",
    "the session's total tokens across every request",
    "failing tool calls summed over every tool",
    "provider retry episodes (consecutive failed attempts count once)",
    "decision points that asked for input, summed over every source",
    "the model the session ran on",
];

/// The tool league table's column names, driving its header and its help
pub const TOOLS_COLUMNS: [&str; 5] = ["tool", "calls", "fails", "fail%", "p50 ms"];

/// What each tool column means, index-aligned with [`TOOLS_COLUMNS`]
const TOOLS_BLURBS: [&str; 5] = [
    "the tool's name, merged across the filtered fleet",
    "how many times the tool was called",
    "how many of those calls failed",
    "fails as a share of calls",
    "the median of the per-session median durations (the honest merge)",
];

/// The signature list's column names, driving its header and its help
pub const SIGNATURES_COLUMNS: [&str; 3] = ["tool", "count", "signature"];

/// What each signature column means, index-aligned with [`SIGNATURES_COLUMNS`]
const SIGNATURES_BLURBS: [&str; 3] = [
    "the tool whose failing results normalized to this cluster",
    "the cluster's weight across the filtered fleet",
    "the normalized signature's first line (`+N lines` marks more; enter opens all of it)",
];

/// The detector table's column names, driving its header and its help
pub const DETECTORS_COLUMNS: [&str; 10] = [
    "technique",
    "leads",
    "fact",
    "likely",
    "unsure",
    "untrust",
    "confirmed",
    "disputed",
    "neutral",
    "ignored",
];

/// What each detector column means, index-aligned with [`DETECTORS_COLUMNS`]
const DETECTORS_BLURBS: [&str; 10] = [
    "the ATT&CK technique the detector seeded (dropped ids merge into `other`)",
    "how many seeded leads carried the technique",
    "leads the detector rated `fact` (directly observed)",
    "leads rated `likely`",
    "leads rated `unsure`",
    "leads rated `untrusted` (easily forged evidence)",
    "leads whose thread verdict confirmed the technique",
    "leads whose thread verdict disputed it",
    "leads discussed without a verdict either way",
    "leads nobody ever replied to or ruled on",
];

/// The stuck-tool-loop flag, explained once and embedded where it renders
const LOOP_FLAG: &str = "**loop!** — three or more consecutive identical failure signatures \
by one agent on one tool: the agent reran a broken call without changing anything, the \
classic stuck tool loop. Identical means byte-equal after normalization, so a changed \
path or count does not reset the streak.";

/// The sharing tiers, explained once and embedded on every tier surface
const TIERS: &str = "**Tiers** — level 0 (the default) is strictly content-free: counts, \
token totals, histograms, enum tallies, timestamps, and hashes; the only strings are tool \
names, event kinds, enum values, role names, model/version ids, and technique ids. Level 1 \
(opt-in) adds two prose classes, both deny-token-redacted and individually droppable on \
review: normalized failure signatures (bounded multi-line prefixes of *failing* tool \
results) and the forum end-state's question titles and task descriptions. Post and comment \
bodies never ship at any level.";

/// The auto-default measure, explained once for the stuck screen
const AUTO_DEFAULT: &str = "**auto-default rate** — the share of decision points the \
harness answered with its built-in default because no analyst was attached to ask. A high \
rate means runs are flying unattended past questions a human was expected to settle.";

/// Bundle merge semantics, explained once for the fleet screen
const MERGE: &str = "**Merging** — exploring several bundles merges their sessions and \
dedupes on the (session id, created-at) pair, so the same session exported twice counts \
once; two different exports of one session (different timestamps) stay distinct rows.";

/// The forum-title privacy posture, explained once for title surfaces
const PRIVACY_TITLES: &str = "**Titles** — question titles and task descriptions ship only \
in level-1 bundles, deny-token-redacted and individually droppable at export; level-0 \
bundles show `title not in bundle (privacy)` here. Over a local session store the real \
prose shows regardless, because nothing is exported by exploring.";

/// The explorer's keys, shared by all six screens' pages
const EXPLORE_KEYS: &str = "## Keys

- `1`-`6` jump to a screen, `Tab`/`Shift-Tab` cycle them
- `Up`/`Down` (or `k`/`j`) move the cursor on the fleet, signatures, stuck,
  and detectors screens and scroll the tools and economics screens
- `PgUp`/`PgDn` page the cursor lists and scroll the stuck/tools/economics pages
- `Enter` opens the cursor's pop-out on the fleet, signatures, stuck, and
  detectors screens (over a local store the fleet drills into the session)
- `i` opens the session-detail pop-out in both modes
- `f` cycles the harness-version filter, `r` rescans a local store
- `Space` opens the shortcut palette, `?` (or `Space`, `h`) this help
- `Esc` closes the top layer -- pop-out, palette, then the app; `q` quits";

/// Renders a column-explanation section from aligned name/blurb arrays
///
/// # Arguments
///
/// * `columns` - The column names, exactly as the table header renders them
/// * `blurbs` - The explanations, index-aligned with the names
fn columns_section(columns: &[&str], blurbs: &[&str]) -> String {
    // the arrays are const-length-matched; assert against future drift
    assert_eq!(columns.len(), blurbs.len(), "every column needs a blurb");
    let mut section = String::from("## Columns\n\n");
    // one bullet per column, the name exactly as the header spells it
    for (column, blurb) in columns.iter().zip(blurbs) {
        section.push_str(&format!("- **{column}** — {blurb}\n"));
    }
    section
}

/// The explorer screen's help page, index-aligned with its screen order
///
/// # Arguments
///
/// * `index` - The screen's index: fleet, tools, signatures, stuck,
///   detectors, economics
pub fn explore_help(index: usize) -> String {
    match index {
        // fleet overview
        0 => format!(
            "# Fleet\n\nEvery session in the open source, after the version filter: the \
             headline totals on top, one row per session underneath. Split lines appear \
             only when harness versions or models actually disagree (the per-session \
             manifest version outranks the exporting tool's). Costs are estimated at \
             render time from the local pricing table, so bundles never carry dollars.\n\n\
             Over a local store `Enter` drills into the session's replay (agent stats and \
             gantt); over bundles it opens the bundle-safe session detail instead -- \
             bundles carry no messages to replay, by design.\n\n{}\n{}\n\n{}",
            columns_section(&FLEET_COLUMNS, &FLEET_BLURBS),
            MERGE,
            EXPLORE_KEYS,
        ),
        // tool league table
        1 => format!(
            "# Tools\n\nThe per-tool league table, merged across the filtered fleet and \
             ordered by call weight. The *offered, never called* list is the exact set \
             difference against the tools-offered manifest, shown only for sessions whose \
             stores carry one -- older stores list nothing rather than a weaker claim.\n\n\
             {}\n{}",
            columns_section(&TOOLS_COLUMNS, &TOOLS_BLURBS),
            EXPLORE_KEYS,
        ),
        // failure signatures
        2 => format!(
            "# Signatures\n\nLevel-1 failure clusters: normalized, defanged, \
             number-scrubbed, path-collapsed prefixes of *failing* tool results (up to 16 \
             lines / 1,500 chars), merged by exact equality across the filtered fleet and \
             ranked by count. Level-0 bundles ship none, so this screen can be empty by \
             privacy choice rather than by health. `Enter` opens the cursor's full \
             multi-line signature with its session spread and the recipes in play.\n\n\
             {}\n{}\n\n{}",
            columns_section(&SIGNATURES_COLUMNS, &SIGNATURES_BLURBS),
            LOOP_FLAG,
            EXPLORE_KEYS,
        ),
        // stuck points
        3 => format!(
            "# Stuck\n\nWhere runs stopped moving: decision-point asks by source, \
             termination classes, context pressure, scrubs and degraded reports, resume \
             interruptions, and provider stalls (retry episodes, backoff time, pauses by \
             kind, runs that died paused).\n\n{}\n\nThe forum end-state at the bottom is \
             four explorable rows -- unsolved questions, needs evidence, blocked \
             questions, tasks unmet. `Up`/`Down` picks a row and `Enter` lists the \
             matching entries grouped by session: who seeded each one (detector class, \
             agent role, or user), its techniques, revision, tasks, replies, and whether \
             anything ever touched it. Bundles predating per-entry rows can only show \
             their counts.\n\n{}\n\n{}",
            AUTO_DEFAULT, PRIVACY_TITLES, EXPLORE_KEYS,
        ),
        // detector outcomes
        4 => format!(
            "# Detectors\n\nHow preseeded detector leads fared, per technique: lead \
             counts, the detector's own confidence spread, and the thread verdicts \
             analysts or agents reached. `Enter` opens the technique's per-lead pop-out \
             (shape, confidence, suspicion, sites, artifact class, source, verdict; the \
             seeded post's prose only over a local store). *exact* marks leads folded \
             from structured provenance rather than template parsing.\n\nThe *never \
             fired* list names rostered registry detections with zero hits fleet-wide -- \
             it appears only for sessions that recorded a roster, so it is exact, never \
             inferred. A detection that failed every rostered pass is listed apart as \
             *failed every pass*: it is broken, not evidence of a clean miss.\n\n{}\n{}",
            columns_section(&DETECTORS_COLUMNS, &DETECTORS_BLURBS),
            EXPLORE_KEYS,
        ),
        // token economics
        _ => format!(
            "# Economics\n\nWhere the tokens went: the input/output/cache/reasoning \
             split, the estimated dollars caching saved (computed at render time from \
             the local pricing table), tokens by agent role, and the busiest hours in \
             local time. Bars are proportional to the heaviest row.\n\n{EXPLORE_KEYS}",
        ),
    }
}

/// The export wizard screen's help page, index-aligned with its walk order
///
/// # Arguments
///
/// * `index` - The screen's index: sessions, tier, review, preview, write
pub fn export_help(index: usize) -> String {
    match index {
        // session picker
        0 => "# Sessions\n\nPick the sessions to fold into one bundle. The list comes \
              from the same read-only background scanner the usage TUI runs; nothing \
              here writes to a session store.\n\n## Keys\n\n- `Up`/`Down` (or `k`/`j`) \
              move, `Space` toggles the cursor's session\n- `a` selects everything or \
              nothing, `r` rescans\n- `Enter` continues to the tier choice\n- `q`/`Esc` \
              quit"
            .to_owned(),
        // tier choice
        1 => format!(
            "# Tier\n\n{TIERS}\n\nThe byte line shows both tiers' exact rendered sizes \
             for the current selection and what level 1 adds -- bytes, signatures, \
             titles -- so the cost of sharing more is a number, not a guess.\n\n## Keys\
             \n\n- `0`/`1` (or `Left`/`Right`) choose the level\n- `Enter` continues to \
             review\n- `Esc` back to the picker, `q` quits",
        ),
        // redaction review
        2 => format!(
            "# Review\n\nEverything prose that would ship at level 1, after deny-token \
             redaction: each failure signature next to its tool, each forum title with \
             its entry facts, and each non-native technique id. What you drop here \
             vanishes from every position in the bundle -- a dropped id merges into \
             `other`, a dropped title leaves its row's counts intact.\n\n{LOOP_FLAG}\n\n\
             {PRIVACY_TITLES}\n\n## Keys\n\n- `Up`/`Down` (or `k`/`j`) move, `Space` \
             drops or keeps the cursor's item\n- `v` (or `Right`) opens a signature's \
             full multi-line pop-out\n- `0` falls back to level 0 entirely\n- `Enter` \
             continues to the preview\n- `Esc` back to the tier choice, `q` quits",
        ),
        // exact-bytes preview
        3 => format!(
            "# Preview\n\nThe exact bytes that will be written -- nothing shown is \
             withheld and nothing unshown ships. `t` re-finalizes the bundle at the \
             other tier in place (`0`/`1` set it directly), keeping every drop, and the \
             header shows the exact byte delta between the tiers. Switching up to level \
             1 warns until its signatures and titles have been seen on the review \
             screen.\n\n{TIERS}\n\n## Keys\n\n- `t` toggles the tier, `0`/`1` set it\n- \
             `Up`/`Down`/`PgUp`/`PgDn`/`Home` scroll\n- `Enter` writes exactly the shown \
             bytes\n- `Esc` back to review, `q` quits",
        ),
        // written summary
        _ => "# Write\n\nThe bundle is on disk. The summary names the path, the \
              sessions, the level, and the SHA-256 of the exact bytes written (before \
              any gzip framing), so what left the machine can be certified byte for \
              byte. The same line prints to stderr headlessly under `--yes`.\n\n## Keys\
              \n\n- any key leaves the wizard (`?` reopens this page first)"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rendered table column is explained on its screen's help page
    #[test]
    fn help_covers_every_column() {
        // (screen index, its column names) for every explorer table
        let tables: [(usize, &[&str]); 4] = [
            (0, &FLEET_COLUMNS),
            (1, &TOOLS_COLUMNS),
            (2, &SIGNATURES_COLUMNS),
            (4, &DETECTORS_COLUMNS),
        ];
        for (index, columns) in tables {
            let help = explore_help(index);
            for column in columns {
                assert!(
                    help.contains(column),
                    "screen {index}'s help must explain the `{column}` column"
                );
            }
        }
    }

    /// The signature screens' pages explain the loop flag by name
    #[test]
    fn loop_flag_is_explained_where_it_renders() {
        // the explorer's signature list and the wizard's review both flag it
        assert!(explore_help(2).contains("loop!"));
        assert!(export_help(2).contains("loop!"));
    }

    /// Every screen in both TUIs has a substantial, titled help page
    #[test]
    fn every_screen_has_help() {
        for index in 0..6 {
            let help = explore_help(index);
            assert!(help.starts_with("# "), "explore {index} must be titled");
            assert!(help.len() > 200, "explore {index} must be substantial");
            assert!(help.contains("## Keys"), "explore {index} must list keys");
        }
        for index in 0..5 {
            let help = export_help(index);
            assert!(help.starts_with("# "), "export {index} must be titled");
            assert!(help.len() > 200, "export {index} must be substantial");
            assert!(help.contains("## Keys"), "export {index} must list keys");
        }
    }

    /// The tier posture is spelled out everywhere tiers are chosen or shown
    #[test]
    fn tier_pages_carry_the_title_posture() {
        for help in [export_help(1), export_help(3)] {
            assert!(
                help.contains("never ship"),
                "the body posture must be stated"
            );
            assert!(
                help.contains("question titles"),
                "the title posture must be stated"
            );
        }
    }
}
