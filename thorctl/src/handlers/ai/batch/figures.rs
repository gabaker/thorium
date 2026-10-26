//! Regenerate the evaluation progression figures from the docs' Scoreboard
//!
//! This is a byte-for-byte port of the retired `api/docs/gen_eval_figures.py`:
//! it parses the pinned Scoreboard table in evaluation.md, recomputes the
//! pooled per-run aggregates (cross-checking every row's printed rate), and
//! emits two self-contained SVGs plus the aggregate table spliced between the
//! eval-figures markers. Writes are skipped when bytes are unchanged and the
//! output embeds no timestamps, so re-runs are idempotent.
//!
//! Port rules (byte-identical output to the Python is a hard requirement):
//!
//! - Every arithmetic expression keeps the Python's operation order; Rust
//!   `{:.N}` and Python `{:.Nf}` are both correctly-rounded fixed conversions,
//!   so identical f64 in means identical bytes out.
//! - Python `round(x)` / `round(x, 1)` are banker's rounding: `round(x)` maps
//!   to `f64::round_ties_even` and `round(x, 1)` to a format-roundtrip
//!   (`format!("{x:.1}").parse()`); `f64::round` (half away from zero) is
//!   never used.
//! - `num(v)` is `format!("{v:.2}")` with trailing `'0'`s then a trailing
//!   `'.'` trimmed; Python `"{:,}"` / `"{:,.2f}"` thousands grouping maps to
//!   the `thousands` / `money` helpers.
//! - String ops are code-point based (`chars()`), matching Python `len` and
//!   slicing for the heatmap's middle-ellipsis labels.
//! - Heatmap family order is first-seen insertion order stably sorted by a
//!   casefold key; `to_lowercase` stands in for `str.casefold` (equivalent for
//!   ASCII names; they differ on special casefolds like `ß` -> `ss`).
//! - Unicode literals are verbatim: em dash `—`, en dash `–` (empty heatmap
//!   cells), U+2212 `−` (negative deltas), `·` separators, `→` in the heatmap
//!   subtitle, `…` ellipsis, and `Σ` in the footers.
//! - SVG attribute order and two-space indentation match the Python helpers
//!   (`text`/`rect`/`hline`/`bar_path`/`svg_open`) exactly, with a trailing
//!   newline after `</svg>`; `esc()` escapes `&` `<` `>` `"` in that order.
//! - The Python's inline regexes are hand-rolled ASCII matchers (Python `\d`
//!   also matches non-ASCII digits; the docs are ASCII so the sets agree).
//! - The Python reads via `Path.read_text` (universal newlines); this port
//!   reads bytes as-is, so the docs must stay LF-only (they are).
//! - The Python prints partial-run warnings to stderr inside `aggregate`; the
//!   port returns them for `handle` to print before any stdout line, keeping
//!   each stream's bytes identical.
//! - Errors carry the Python's message core (`evaluation.md:{lineno}:
//!   {reason}`, the no-Scoreboard/no-header messages, the marker-count error)
//!   so failures stay greppable across both implementations.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use thorium::Error;

use crate::args::ai::BatchFigures;

/// The marker opening the machine-owned block in evaluation.md
const BEGIN: &str = "<!-- eval-figures:begin -->";
/// The marker closing the machine-owned block in evaluation.md
const END: &str = "<!-- eval-figures:end -->";

/// The pinned Scoreboard header: any column change must fail loudly here
const HEADER: [&str; 12] = [
    "Run",
    "Family",
    "Static GT",
    "Replicated",
    "Partial",
    "Missed",
    "Wrong",
    "Rate",
    "Tokens",
    "Cost",
    "Novel",
    "CISA rate",
];

/// The dark card surface color (dataviz palette, validated for the dark card)
const CARD: &str = "#1a1a19";
/// The hairline ring around the card
const RING: &str = "rgba(255,255,255,0.10)";
/// The primary ink color
const INK: &str = "#ffffff";
/// The secondary ink color
const INK2: &str = "#c3c2b7";
/// The muted ink color for axes and footers
const MUTED: &str = "#898781";
/// The dark ink used on bright heatmap cells
const DARK_INK: &str = "#0b0b0b";
/// The gridline color
const GRID: &str = "#2c2c2a";
/// The axis baseline color
const BASE: &str = "#383835";
/// The replication-rate line hue
const RATE_HUE: &str = "#3987e5";
/// The cost bar hue
const COST_HUE: &str = "#d95926";
/// Heatmap valence ramp: recessive blue = low -> vivid green = high coverage,
/// with lightness rising monotonically and chroma nearly doubling end to end
/// (0.11 -> 0.20, one imperceptible 0.007 dip at step 3) so the eye lands on
/// the high cells and the hue distinction survives red/green colorblindness
/// (deutan hue spread 161 deg vs 4 deg for a red->green ramp; worst adjacent
/// CVD dE 8.3; dark end 2.21:1)
const RAMP: [&str; 6] = [
    "#30508f", "#396aae", "#2b8cbc", "#16ad88", "#4bca4b", "#7deb60",
];
/// The font stack shared by both figures
const FONT: &str = "system-ui, -apple-system, 'Segoe UI', sans-serif";

/// One Scoreboard row: a single family scored in a single run
#[derive(Debug)]
struct Row {
    /// The run number this row was scored in
    run: i64,
    /// The family display name from its markdown link
    family: String,
    /// The number of static ground-truth items
    gt: i64,
    /// The number of fully replicated items
    rep: i64,
    /// The number of partially replicated items
    par: i64,
    /// The number of missed items (parsed like the Python but unused here)
    #[allow(dead_code)]
    missed: i64,
    /// The number of wrong claims (parsed like the Python but unused here)
    #[allow(dead_code)]
    wrong: i64,
    /// The recomputed replication rate in percent
    rate: f64,
    /// The estimated dollar cost of this row's run
    cost: f64,
    /// The family slug from the link target, when it is a families/ page
    slug: Option<String>,
    /// The verbatim Novel cell (`—` or a count)
    novel_cell: String,
    /// The verbatim CISA rate cell (`—` or a percent)
    cisa_cell: String,
}

/// Pooled aggregates for one complete run
#[derive(Debug)]
struct RunAgg {
    /// The run number
    run: i64,
    /// The number of families scored in this run
    n: usize,
    /// The pooled replication rate in percent
    rate: f64,
    /// The total estimated cost of this run
    cost: f64,
    /// The rate delta against the prior comparable run in percentage points
    delta: Option<f64>,
}

/// Build the loud per-row parse error the Python's `die` printed
///
/// # Arguments
///
/// * `lineno` - The 1-indexed line number the malformed row sits on
/// * `reason` - Why the row failed to parse
fn die(lineno: usize, reason: &str) -> Error {
    Error::new(format!("evaluation.md:{lineno}: {reason}"))
}

/// Split a markdown table line into stripped cells
///
/// # Arguments
///
/// * `line` - The table line to split
fn cells_of(line: &str) -> Vec<&str> {
    // split on every pipe like the Python
    let parts: Vec<&str> = line.split('|').collect();
    // dropping the first and last part leaves nothing without two pipes
    if parts.len() <= 2 {
        return Vec::new();
    }
    // keep the interior parts with their whitespace stripped
    parts[1..parts.len() - 1].iter().map(|c| c.trim()).collect()
}

/// Split a family cell's markdown link into its display name and target
///
/// # Arguments
///
/// * `cell` - The family cell to match in full
fn split_family_link(cell: &str) -> Option<(&str, &str)> {
    // the display name runs from '[' to the first ']' and must be non-empty
    let rest = cell.strip_prefix('[')?;
    let close = rest.find(']')?;
    let label = &rest[..close];
    if label.is_empty() {
        return None;
    }
    // the link target is parenthesized, ends the cell, and holds no ')'
    let target = rest[close + 1..].strip_prefix('(')?.strip_suffix(')')?;
    if target.contains(')') {
        return None;
    }
    Some((label, target))
}

/// Match the family regex `\[([^\]]+)\]\([^)]*\)`, returning the display name
///
/// # Arguments
///
/// * `cell` - The family cell to match in full
fn parse_family_link(cell: &str) -> Option<&str> {
    split_family_link(cell).map(|(label, _)| label)
}

/// Extract the family slug from a family cell's `./families/<slug>.md` target
///
/// # Arguments
///
/// * `cell` - The family cell holding the markdown link
fn parse_family_slug(cell: &str) -> Option<&str> {
    // the label may itself contain parentheses, so split on the link shape
    let (_, target) = split_family_link(cell)?;
    // only families/ pages carry a scoring-JSON slug
    target
        .strip_prefix("./families/")
        .and_then(|rest| rest.strip_suffix(".md"))
        .filter(|slug| !slug.is_empty())
}

/// Match `\d+(?:\.\d+)?` over a whole string (ASCII digits)
///
/// # Arguments
///
/// * `s` - The candidate number to match in full
fn is_simple_number(s: &str) -> bool {
    match s.split_once('.') {
        // no decimal point: one or more digits
        None => !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()),
        // decimal point: digits required on both sides and no second point
        Some((int, frac)) => {
            !int.is_empty()
                && !frac.is_empty()
                && int.bytes().all(|b| b.is_ascii_digit())
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

/// Match the rate regex `(\d+(?:\.\d+)?)%`, returning the number
///
/// # Arguments
///
/// * `cell` - The rate cell to match in full
fn parse_rate_cell(cell: &str) -> Option<&str> {
    // strip the percent sign and require a simple number before it
    let body = cell.strip_suffix('%')?;
    is_simple_number(body).then_some(body)
}

/// Match the cost regex `\$([\d,]+\.\d{2})`, returning the dollar amount
///
/// # Arguments
///
/// * `cell` - The cost cell to match in full
fn parse_cost_cell(cell: &str) -> Option<&str> {
    // strip the dollar sign and split on the single decimal point
    let body = cell.strip_prefix('$')?;
    let (int, frac) = body.split_once('.')?;
    // the integer part is one or more digits/commas, the cents exactly two digits
    let int_ok = !int.is_empty() && int.bytes().all(|b| b.is_ascii_digit() || b == b',');
    let frac_ok = frac.len() == 2 && frac.bytes().all(|b| b.is_ascii_digit());
    (int_ok && frac_ok).then_some(body)
}

/// Match the Novel regex `—|\d+`
///
/// # Arguments
///
/// * `cell` - The Novel cell to match in full
fn is_novel_cell(cell: &str) -> bool {
    cell == "—" || (!cell.is_empty() && cell.bytes().all(|b| b.is_ascii_digit()))
}

/// Match the CISA rate regex `—|\d+(?:\.\d+)?%`
///
/// # Arguments
///
/// * `cell` - The CISA rate cell to match in full
fn is_cisa_cell(cell: &str) -> bool {
    cell == "—" || cell.strip_suffix('%').is_some_and(is_simple_number)
}

/// Parse and validate every Scoreboard row out of evaluation.md
///
/// # Arguments
///
/// * `md` - The full evaluation.md content
fn parse_scoreboard(md: &str) -> Result<Vec<Row>, Error> {
    // split into lines (the docs are LF-only; see the newline port rule)
    let lines: Vec<&str> = md.lines().collect();
    // locate the scoreboard section, then its pinned header within it
    let start = lines
        .iter()
        .position(|line| *line == "## Scoreboard")
        .ok_or_else(|| Error::new("no '## Scoreboard' heading in evaluation.md"))?;
    let head = (start..lines.len())
        .find(|&i| cells_of(lines[i]) == HEADER)
        .ok_or_else(|| Error::new("pinned Scoreboard header not found (columns changed?)"))?;
    // the |---| separator must sit directly under the header
    if head + 1 >= lines.len() || !lines[head + 1].trim_start().starts_with("|-") {
        return Err(die(
            head + 2,
            "expected the |---| separator under the header",
        ));
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut seen: HashSet<(i64, String)> = HashSet::new();
    // consume consecutive table rows; the first non-pipe line ends the table
    for (i, line) in lines.iter().enumerate().skip(head + 2) {
        let line = *line;
        if !line.trim_start().starts_with('|') {
            break;
        }
        let lineno = i + 1;
        let cells = cells_of(line);
        if cells.len() != 12 {
            return Err(die(
                lineno,
                &format!("expected 12 cells, found {}", cells.len()),
            ));
        }
        // family is a markdown link to its page; keep the display name
        let fam = parse_family_link(cells[1]).ok_or_else(|| {
            die(
                lineno,
                &format!("family cell is not a markdown link: '{}'", cells[1]),
            )
        })?;
        let rate = parse_rate_cell(cells[7])
            .ok_or_else(|| die(lineno, &format!("malformed rate cell: '{}'", cells[7])))?;
        let cost = parse_cost_cell(cells[9])
            .ok_or_else(|| die(lineno, &format!("malformed cost cell: '{}'", cells[9])))?;
        // extra columns: Novel is a count or an em-dash, CISA rate a percent or an
        // em-dash; both are format-checked only (the figures do not consume them,
        // and the CISA rate cannot be recomputed from Scoreboard data alone)
        if !is_novel_cell(cells[10]) {
            return Err(die(
                lineno,
                &format!("malformed Novel cell: '{}'", cells[10]),
            ));
        }
        if !is_cisa_cell(cells[11]) {
            return Err(die(
                lineno,
                &format!("malformed CISA rate cell: '{}'", cells[11]),
            ));
        }
        // parse the six integer count cells in the Python's evaluation order
        let mut counts = [0i64; 6];
        for (slot, k) in [0usize, 2, 3, 4, 5, 6].into_iter().enumerate() {
            counts[slot] = cells[k].parse().map_err(|_| {
                // approximate the Python ValueError text with plain quoting
                die(
                    lineno,
                    &format!(
                        "non-integer count cell: invalid literal for int() with base 10: '{}'",
                        cells[k]
                    ),
                )
            })?;
        }
        let [run, gt, rep, par, missed, wrong] = counts;
        if gt <= 0 {
            return Err(die(lineno, "static GT must be a positive count"));
        }
        // recompute the rate and require the printed cell to be a valid rounding
        // of it (half-up and half-even both land within 0.05 of the exact value)
        let computed = (rep as f64 + 0.5 * par as f64) / gt as f64 * 100.0;
        let printed: f64 = rate
            .parse()
            .map_err(|_| die(lineno, &format!("malformed rate cell: '{}'", cells[7])))?;
        if (computed - printed).abs() > 0.05 + 1e-9 {
            return Err(die(
                lineno,
                &format!(
                    "rate cross-check failed: printed {}, computed {computed:.1}%",
                    cells[7]
                ),
            ));
        }
        // reject a second row for the same (run, family) pair
        if !seen.insert((run, fam.to_string())) {
            return Err(die(lineno, &format!("duplicate row for run {run} / {fam}")));
        }
        // keep the recomputed rate and the de-comma'd dollar cost
        let cost: f64 = cost
            .replace(',', "")
            .parse()
            .map_err(|_| die(lineno, &format!("malformed cost cell: '{}'", cells[9])))?;
        rows.push(Row {
            run,
            family: fam.to_string(),
            gt,
            rep,
            par,
            missed,
            wrong,
            rate: computed,
            cost,
            slug: parse_family_slug(cells[1]).map(str::to_string),
            novel_cell: cells[10].to_string(),
            cisa_cell: cells[11].to_string(),
        });
    }
    if rows.is_empty() {
        return Err(Error::new("Scoreboard table has no rows"));
    }
    Ok(rows)
}

/// Recompute each row's Novel and CISA rate cells from its scoring JSON
///
/// These are the only Scoreboard cells the table cannot cross-check against
/// itself, so they were historically format-checked only (an audit called them
/// trust-me cells). Given the scoring directory, every row with an
/// `run<run>-<slug>.json` is recomputed exactly like the audit census did:
/// Novel is the count of verified novel findings and the CISA rate is
/// (replicated + 0.5 partial) over the static items whose source cites CISA.
/// Rows without a scoring JSON are skipped and counted for the caller to
/// report, so coverage is never silently narrower than it looks.
///
/// # Arguments
///
/// * `rows` - The parsed Scoreboard rows
/// * `dir` - The scoring directory holding the run<run>-<slug>.json files
fn verify_scoring_cells(rows: &[Row], dir: &Path) -> Result<(usize, usize), Error> {
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut diffs: Vec<String> = Vec::new();
    for row in rows {
        // verification keys on the slug in the family link target
        let Some(slug) = &row.slug else {
            diffs.push(format!(
                "run {} {}: family link is not a ./families/<slug>.md target",
                row.run, row.family
            ));
            continue;
        };
        // a row without a scoring JSON predates the artifact layout: skip it
        let path = dir.join(format!("run{}-{slug}.json", row.run));
        let Ok(raw) = fs::read_to_string(&path) else {
            skipped += 1;
            continue;
        };
        let json: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|err| Error::new(format!("failed to parse {}: {err}", path.display())))?;
        let result = &json["result"];
        let Some(items) = result["items"].as_array() else {
            return Err(Error::new(format!(
                "{}: no result.items array",
                path.display()
            )));
        };
        // novel: the verified findings folded into the JSON after adjudication
        let novel = result["novel"].as_array().map_or(0, Vec::len);
        // cisa: credit over the static items whose source cites a CISA advisory
        let mut cisa = 0u64;
        let mut credit = 0.0f64;
        for item in items {
            let scope = item["scope"].as_str().unwrap_or("");
            let source = item["source"].as_str().unwrap_or("");
            if scope != "static" || !source.contains("CISA") {
                continue;
            }
            cisa += 1;
            match item["verdict"].as_str().unwrap_or("") {
                "replicated" => credit += 1.0,
                "partial" => credit += 0.5,
                _ => (),
            }
        }
        // the published Novel cell must equal the recomputed count ('—' is 0)
        let published_novel: usize = match row.novel_cell.as_str() {
            "—" => 0,
            cell => cell.parse().unwrap_or(usize::MAX),
        };
        if published_novel != novel {
            diffs.push(format!(
                "run {} {slug}: Novel cell '{}' != recomputed {novel}",
                row.run, row.novel_cell
            ));
        }
        // the published CISA cell must round to the recomputed rate ('—' means
        // the family has no CISA-sourced static items)
        match (row.cisa_cell.as_str(), cisa) {
            ("—", 0) => (),
            ("—", _) => diffs.push(format!(
                "run {} {slug}: CISA cell '—' but {cisa} CISA item(s) recomputed",
                row.run
            )),
            (cell, 0) => diffs.push(format!(
                "run {} {slug}: CISA cell '{cell}' but no CISA items in the scoring JSON",
                row.run
            )),
            (cell, _) => {
                let rate = credit / cisa as f64 * 100.0;
                let printed: f64 = cell.trim_end_matches('%').parse().unwrap_or(f64::NAN);
                if !((rate - printed).abs() <= 0.06) {
                    diffs.push(format!(
                        "run {} {slug}: CISA cell '{cell}' != recomputed {rate:.1}%",
                        row.run
                    ));
                }
            }
        }
        checked += 1;
    }
    // every stale cell is reported at once so one pass fixes them all
    if !diffs.is_empty() {
        return Err(Error::new(format!(
            "scoring recompute found {} stale Novel/CISA cell(s):\n  {}",
            diffs.len(),
            diffs.join("\n  ")
        )));
    }
    Ok((checked, skipped))
}

/// Pool per-run aggregates, excluding runs that cover only part of the corpus
///
/// The corpus a run must cover is the union of families seen up to and
/// including that run — so a family added in a later run never retroactively
/// reclassifies earlier complete runs as partial, while a mid-cycle run that
/// has not yet scored every family is still excluded until its rows land.
/// The delta is suppressed whenever the scored corpus changed since the prior
/// aggregated run, because pooled rates over different corpora do not compare.
/// Partial-run warnings are returned for the caller to print to stderr (the
/// Python printed them here).
///
/// # Arguments
///
/// * `rows` - The parsed Scoreboard rows to pool
fn aggregate(rows: &[Row]) -> (Vec<RunAgg>, Vec<String>) {
    // bucket the rows by run, iterated in ascending run order
    let mut by_run: BTreeMap<i64, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        by_run.entry(r.run).or_default().push(r);
    }
    let mut aggs: Vec<RunAgg> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut prev: Option<f64> = None;
    let mut prev_families: Option<HashSet<&str>> = None;
    let mut corpus: HashSet<&str> = HashSet::new();
    for (run, batch) in &by_run {
        // the corpus grows as new families join at this run
        corpus.extend(batch.iter().map(|r| r.family.as_str()));
        let families: HashSet<&str> = batch.iter().map(|r| r.family.as_str()).collect();
        // a partially scored run would skew the pooled rate: warn and skip
        if families != corpus {
            warnings.push(format!(
                "warning: run {run} covers {} of {} families; excluded from aggregates",
                batch.len(),
                corpus.len()
            ));
            continue;
        }
        // pool the integer counts over the whole batch
        let rep: i64 = batch.iter().map(|r| r.rep).sum();
        let par: i64 = batch.iter().map(|r| r.par).sum();
        let gt: i64 = batch.iter().map(|r| r.gt).sum();
        let rate = (rep as f64 + 0.5 * par as f64) / gt as f64 * 100.0;
        // the pooled cost sums left to right like the Python
        let cost = batch.iter().fold(0.0, |acc, r| acc + r.cost);
        // delta over the displayed (rounded) rates so the table stays self-consistent,
        // suppressed across corpus changes (a 12-family rate vs a 24-family rate)
        let delta = match (prev, &prev_families) {
            (Some(prev_rate), Some(prev_fams)) if families == *prev_fams => {
                Some(round1(rate) - round1(prev_rate))
            }
            _ => None,
        };
        aggs.push(RunAgg {
            run: *run,
            n: batch.len(),
            rate,
            cost,
            delta,
        });
        prev = Some(rate);
        prev_families = Some(families);
    }
    (aggs, warnings)
}

/// Format a coordinate with trailing zeros stripped for byte-stable svg
///
/// # Arguments
///
/// * `v` - The coordinate to format
fn num(v: f64) -> String {
    // fixed two decimals, then trim trailing zeros and any bare point
    let fixed = format!("{v:.2}");
    fixed
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// Round to one decimal exactly like Python's `round(v, 1)`
///
/// # Arguments
///
/// * `v` - The value to round
fn round1(v: f64) -> f64 {
    // the fixed conversion is correctly rounded (ties to even on the exact
    // binary value), and reparsing lands on the same double Python returns
    format!("{v:.1}").parse().unwrap_or(v)
}

/// Group a run of decimal digits in threes like Python's `"{:,}"`
///
/// # Arguments
///
/// * `digits` - The unsigned digit run to group
pub(super) fn group_digits(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        // a comma lands before every later complete group of three
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Format an integer with thousands separators like Python's `f"{v:,}"`
///
/// # Arguments
///
/// * `v` - The integer to format
fn thousands(v: i64) -> String {
    // group the magnitude and restore the sign
    let grouped = group_digits(&v.unsigned_abs().to_string());
    if v < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Format a dollar amount like Python's `f"{v:,.2f}"`
///
/// # Arguments
///
/// * `v` - The dollar amount to format
fn money(v: f64) -> String {
    // fixed two-decimal form first, then comma-group the integer digits
    let fixed = format!("{v:.2}");
    match fixed.split_once('.') {
        Some((int, frac)) => {
            // keep any sign ahead of the grouped digits
            let (sign, digits) = match int.strip_prefix('-') {
                Some(digits) => ("-", digits),
                None => ("", int),
            };
            format!("{sign}{}.{frac}", group_digits(digits))
        }
        // unreachable: a fixed conversion always contains a point
        None => fixed,
    }
}

/// Escape text for svg content and attribute values
///
/// # Arguments
///
/// * `s` - The raw text to escape
fn esc(s: &str) -> String {
    // escape & first so the later entities are not double-escaped
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Linearize one 0-255 srgb channel for luminance math
///
/// # Arguments
///
/// * `v` - The 0-255 channel value
fn channel(v: i64) -> f64 {
    // normalize then apply the srgb transfer curve
    let c = v as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance of a `#rrggbb` color
///
/// # Arguments
///
/// * `color` - The `#rrggbb` color to measure
fn luminance(color: &str) -> f64 {
    // parse each hex pair; the palette constants are always well formed
    let hex = |k: usize| i64::from_str_radix(&color[k..k + 2], 16).unwrap_or(0);
    let (r, g, b) = (hex(1), hex(3), hex(5));
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// WCAG contrast ratio between two `#rrggbb` colors
///
/// # Arguments
///
/// * `a` - The first color
/// * `b` - The second color
fn contrast(a: &str, b: &str) -> f64 {
    // luminance order does not matter: brightest goes on top
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Pick the higher-contrast ink (dark or white) for text on a fill
///
/// # Arguments
///
/// * `fill` - The cell fill color the text sits on
fn cell_ink(fill: &str) -> &'static str {
    // dark ink wins ties exactly like the Python's >= comparison
    if contrast(DARK_INK, fill) >= contrast(INK, fill) {
        DARK_INK
    } else {
        INK
    }
}

/// Bin a rate onto the blue->green ramp (bolder green = higher) exactly
///
/// bin = floor(6 * rate / 100) = (6*rep + 3*par) // gt in integer math, clamped
/// to 5 — so boundary rates like an exact 50.0% land deterministically in the
/// upper bin instead of wherever float rounding happens to drop them.
///
/// # Arguments
///
/// * `rep` - The replicated item count
/// * `par` - The partial item count
/// * `gt` - The static ground-truth item count
fn ramp_color(rep: i64, par: i64, gt: i64) -> &'static str {
    // floor division like Python's //, clamped to the top bin
    let bin = (3 * (2 * rep + par)).div_euclid(gt).min(5);
    // mirror Python's negative-index wrap for the (pathological) negative-count
    // rows that still pass the rate cross-check; bin is never below -1
    RAMP[bin.rem_euclid(6) as usize]
}

/// Emit an svg text element
///
/// # Arguments
///
/// * `x` - The x coordinate
/// * `y` - The y coordinate
/// * `s` - The text content
/// * `size` - The font size in px
/// * `fill` - The text color
/// * `anchor` - The text anchor (only non-"start" values are emitted)
/// * `weight` - The font weight if any
/// * `tnum` - Whether to request tabular numerals
#[allow(clippy::too_many_arguments)]
fn text(
    x: f64,
    y: f64,
    s: &str,
    size: i64,
    fill: &str,
    anchor: &str,
    weight: Option<i64>,
    tnum: bool,
) -> String {
    // the required attributes always lead in this order
    let mut attrs = format!(
        "x=\"{}\" y=\"{}\" font-size=\"{size}\" fill=\"{fill}\"",
        num(x),
        num(y)
    );
    // optional attributes append in the Python's order
    if anchor != "start" {
        attrs.push_str(&format!(" text-anchor=\"{anchor}\""));
    }
    if let Some(weight) = weight {
        attrs.push_str(&format!(" font-weight=\"{weight}\""));
    }
    if tnum {
        attrs.push_str(" style=\"font-variant-numeric: tabular-nums\"");
    }
    format!("  <text {attrs}>{}</text>", esc(s))
}

/// Emit an svg rect
///
/// # Arguments
///
/// * `x` - The x coordinate
/// * `y` - The y coordinate
/// * `w` - The width
/// * `h` - The height
/// * `fill` - The fill color
/// * `rx` - The corner radius if any
fn rect(x: f64, y: f64, w: f64, h: f64, fill: &str, rx: Option<f64>) -> String {
    // the radius is the only optional attribute
    let extra = match rx {
        Some(rx) => format!(" rx=\"{}\"", num(rx)),
        None => String::new(),
    };
    format!(
        "  <rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{fill}\"{extra}/>",
        num(x),
        num(y),
        num(w),
        num(h)
    )
}

/// Emit a 1px solid horizontal hairline
///
/// # Arguments
///
/// * `x1` - The left x coordinate
/// * `x2` - The right x coordinate
/// * `y` - The shared y coordinate
/// * `stroke` - The stroke color
fn hline(x1: f64, x2: f64, y: f64, stroke: &str) -> String {
    format!(
        "  <line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{stroke}\" stroke-width=\"1\"/>",
        num(x1),
        num(y),
        num(x2),
        num(y)
    )
}

/// Emit a bar rounded 4px at the data end only, square at the baseline
///
/// # Arguments
///
/// * `x` - The bar's left edge
/// * `w` - The bar width
/// * `top` - The bar's data-end y coordinate
/// * `base` - The baseline y coordinate
/// * `fill` - The bar color
fn bar_path(x: f64, w: f64, top: f64, base: f64, fill: &str) -> String {
    // shrink the radius for bars too short or narrow to hold it
    let r = 4.0_f64.min((base - top) / 2.0).min(w / 2.0);
    // trace up the left edge, round both top corners, and close at the base
    let d = format!(
        "M {} {} L {} {} Q {} {} {} {} L {} {} Q {} {} {} {} L {} {} Z",
        num(x),
        num(base),
        num(x),
        num(top + r),
        num(x),
        num(top),
        num(x + r),
        num(top),
        num(x + w - r),
        num(top),
        num(x + w),
        num(top),
        num(x + w),
        num(top + r),
        num(x + w),
        num(base)
    );
    format!("  <path d=\"{d}\" fill=\"{fill}\"/>")
}

/// Open the svg root with its accessible title, card surface, and font group
///
/// # Arguments
///
/// * `w` - The pixel width of the figure
/// * `h` - The pixel height of the figure
/// * `label` - The aria-label describing the figure
/// * `title` - The accessible title element content
fn svg_open(w: i64, h: f64, label: &str, title: &str) -> Vec<String> {
    vec![
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{}\" viewBox=\"0 0 {w} {}\" role=\"img\" aria-label=\"{}\">",
            num(h),
            num(h),
            esc(label)
        ),
        format!("  <title>{}</title>", esc(title)),
        format!(
            "  <rect x=\"0.5\" y=\"0.5\" width=\"{}\" height=\"{}\" rx=\"12\" fill=\"{CARD}\" stroke=\"{RING}\" stroke-width=\"1\"/>",
            num(w as f64 - 1.0),
            num(h - 1.0)
        ),
        format!("  <g font-family=\"{FONT}\">"),
    ]
}

/// Place a point label above its marker, flipping below on segment collision
///
/// # Arguments
///
/// * `pts` - Every marker position on the line
/// * `i` - The index of the point being labelled
/// * `top` - The top of the plot band
/// * `bot` - The bottom of the plot band
fn label_y(pts: &[(f64, f64)], i: usize, top: f64, bot: f64) -> f64 {
    let (x, y) = pts[i];
    // check whether the segment toward point j crosses the above-label zone
    let hits_above = |j: usize| -> bool {
        // sample the neighbouring segment's height at the label box edge
        let (ox, oy) = pts[j];
        if ox == x {
            return false;
        }
        let dx = 18.0_f64.min((ox - x).abs());
        y + (oy - y) * (dx / (ox - x).abs()) < y - 8.0
    };
    // the label flips below when either existing neighbour segment collides
    let collides_left = i > 0 && hits_above(i - 1);
    let collides_right = i + 1 < pts.len() && hits_above(i + 1);
    let above = !(collides_left || collides_right);
    let ly = if above { y - 10.0 } else { y + 18.0 };
    // clamp inside the plot band
    ly.max(top + 10.0).min(bot - 4.0)
}

/// Pick the smallest round tick step giving at most six intervals
///
/// # Arguments
///
/// * `max_cost` - The tallest run cost the scale must cover
fn cost_scale(max_cost: f64) -> (f64, i64) {
    // the first candidate step with at most six intervals wins
    let mut step: i64 = 100;
    let mut found = false;
    for cand in [100, 200, 250, 500, 1000, 2000] {
        step = cand;
        if (max_cost * 1.1 / cand as f64).ceil() as i64 <= 6 {
            found = true;
            break;
        }
    }
    // beyond the candidate set, keep doubling the largest step
    if !found {
        while (max_cost * 1.1 / step as f64).ceil() as i64 > 6 {
            step *= 2;
        }
    }
    // never return a zero scale (an all-zero cost column would otherwise divide by it)
    let ticks = (max_cost * 1.1 / step as f64).ceil() as i64;
    (step.max(step * ticks) as f64, step)
}

/// Describe figure 1 chronologically (both measures first run -> last run)
///
/// # Arguments
///
/// * `aggs` - The pooled per-run aggregates
fn progression_alt(aggs: &[RunAgg]) -> String {
    // only the endpoints appear in the description
    let (first, last) = (&aggs[0], &aggs[aggs.len() - 1]);
    format!(
        "Aggregate progression: replication rate {:.1}% (run {}) to {:.1}% (run {}); total cost per run ${} to ${}",
        first.rate,
        first.run,
        last.rate,
        last.run,
        thousands(first.cost.round_ties_even() as i64),
        thousands(last.cost.round_ties_even() as i64)
    )
}

/// Render the aggregate figure: rate line over cost bars on a shared run axis
///
/// # Arguments
///
/// * `aggs` - The pooled per-run aggregates to plot
fn emit_progression_svg(aggs: &[RunAgg]) -> String {
    let n = aggs.len();
    let (left, right) = (64.0, 772.0);
    let (a_top, a_bot) = (112.0, 292.0);
    let (b_top, b_bot) = (342.0, 492.0);
    // one x center per run, shared by both panels
    let cx: Vec<f64> = (0..n)
        .map(|i| left + (right - left) * (i as f64 + 0.5) / n as f64)
        .collect();
    let mut out = svg_open(
        800,
        580.0,
        &progression_alt(aggs),
        "Thorctl AI evaluation — aggregate progression",
    );
    out.push(text(
        28.0,
        46.0,
        "Thorctl AI evaluation — aggregate progression",
        15,
        INK,
        "start",
        Some(600),
        false,
    ));
    out.push(text(
        28.0,
        66.0,
        "Pooled replication rate and total estimated cost over each run's scored corpus",
        12,
        INK2,
        "start",
        None,
        false,
    ));
    // panel A: the replication rate line on a fixed 0-100% scale
    out.push(text(
        left,
        100.0,
        "Replication rate (%)",
        12,
        INK2,
        "start",
        None,
        false,
    ));
    for tick in (0..=100).step_by(20) {
        let y = a_bot - (a_bot - a_top) * tick as f64 / 100.0;
        out.push(hline(left, right, y, GRID));
        out.push(text(
            56.0,
            y + 4.0,
            &tick.to_string(),
            11,
            MUTED,
            "end",
            None,
            true,
        ));
    }
    let pts: Vec<(f64, f64)> = aggs
        .iter()
        .enumerate()
        .map(|(i, a)| (cx[i], a_bot - (a_bot - a_top) * a.rate / 100.0))
        .collect();
    let path = pts
        .iter()
        .enumerate()
        .map(|(i, (x, y))| format!("{} {} {}", if i == 0 { "M" } else { "L" }, num(*x), num(*y)))
        .collect::<Vec<String>>()
        .join(" ");
    out.push(format!(
        "  <path d=\"{path}\" fill=\"none\" stroke=\"{RATE_HUE}\" stroke-width=\"2\" stroke-linejoin=\"round\" stroke-linecap=\"round\"/>"
    ));
    for (i, (x, y)) in pts.iter().enumerate() {
        // marker with a 2px card-color ring so overlapping the line stays clean
        out.push(format!(
            "  <circle cx=\"{}\" cy=\"{}\" r=\"4.5\" fill=\"{RATE_HUE}\" stroke=\"{CARD}\" stroke-width=\"2\"/>",
            num(*x),
            num(*y)
        ));
        out.push(text(
            *x,
            label_y(&pts, i, a_top, a_bot),
            &format!("{:.1}", aggs[i].rate),
            11,
            INK,
            "middle",
            Some(600),
            true,
        ));
    }
    // panel B: total cost bars from a zero baseline
    out.push(text(
        left,
        330.0,
        "Total cost per run (USD)",
        12,
        INK2,
        "start",
        None,
        false,
    ));
    // track the tallest bar once; the Python recomputes the same max in place
    let mut max_cost = aggs[0].cost;
    for a in &aggs[1..] {
        if a.cost > max_cost {
            max_cost = a.cost;
        }
    }
    let (mut scale_max, step) = cost_scale(max_cost);
    // guarantee >= 16px headroom above the tallest bar: its label always fits
    // above the tip, so no in-bar fallback (which a 24px bar cannot fit) exists
    while (b_bot - b_top) * (1.0 - max_cost / scale_max) < 16.0 {
        scale_max += step as f64;
    }
    let mut tick: i64 = 0;
    while tick <= scale_max as i64 {
        let y = b_bot - (b_bot - b_top) * tick as f64 / scale_max;
        out.push(hline(left, right, y, GRID));
        out.push(text(
            56.0,
            y + 4.0,
            &thousands(tick),
            11,
            MUTED,
            "end",
            None,
            true,
        ));
        tick += step;
    }
    for (i, a) in aggs.iter().enumerate() {
        let top_y = b_bot - (b_bot - b_top) * a.cost / scale_max;
        out.push(bar_path(cx[i] - 12.0, 24.0, top_y, b_bot, COST_HUE));
        out.push(text(
            cx[i],
            top_y - 8.0,
            &format!("${}", thousands(a.cost.round_ties_even() as i64)),
            11,
            INK,
            "middle",
            Some(600),
            true,
        ));
    }
    // the axis baseline sits over the zero gridline
    out.push(hline(left, right, b_bot, BASE));
    // shared run axis under both panels, then the formula footer
    for (i, a) in aggs.iter().enumerate() {
        out.push(text(
            cx[i],
            514.0,
            &format!("Run {}", a.run),
            12,
            INK2,
            "middle",
            None,
            false,
        ));
    }
    out.push(text(left, 552.0, "Rate = (Σ replicated + 0.5 Σ partial) / Σ static GT · Cost = Σ estimated $ per run · Source: Scoreboard table", 11, MUTED, "start", None, false));
    out.push("  </g>".to_string());
    out.push("</svg>".to_string());
    out.join("\n") + "\n"
}

/// Render the per-family heatmap: one cell per family per run
///
/// # Arguments
///
/// * `rows` - Every parsed Scoreboard row
fn emit_heatmap_svg(rows: &[Row]) -> String {
    // distinct runs in ascending order
    let runs: Vec<i64> = rows
        .iter()
        .map(|r| r.run)
        .collect::<std::collections::BTreeSet<i64>>()
        .into_iter()
        .collect();
    // pivot the rows into family -> run -> row (bin needs the integer counts),
    // remembering each family's first-seen position like a Python dict
    let mut order: Vec<&str> = Vec::new();
    let mut pivot: HashMap<&str, HashMap<i64, &Row>> = HashMap::new();
    for r in rows {
        let per_family = pivot.entry(r.family.as_str()).or_insert_with(|| {
            order.push(r.family.as_str());
            HashMap::new()
        });
        per_family.insert(r.run, r);
    }
    // alphabetical rows: positions stay stable run over run for the same corpus
    // (stable sort over first-seen order; to_lowercase stands in for casefold,
    // equivalent for the ASCII family names the docs use)
    let mut families = order;
    families.sort_by_key(|fam| fam.to_lowercase());
    let (gutter_x, grid_x, grid_right) = (176.0, 184.0, 772.0);
    let (cell_h, gap) = (28.0, 2.0);
    let cell_w = (grid_right - grid_x - gap * (runs.len() - 1) as f64) / runs.len() as f64;
    let top = 104.0;
    // the card grows with the corpus: key band and footer hang off the last row
    let grid_bot = top + families.len() as f64 * (cell_h + gap) - gap;
    let key_y = grid_bot + 16.0;
    let footer_y = key_y + 36.0;
    let height = footer_y + 28.0;
    let alt = format!(
        "Heatmap of replication rate for {} families across {} runs, rows alphabetical by family",
        families.len(),
        runs.len()
    );
    let mut out = svg_open(800, height, &alt, "Per-family replication rate by run (%)");
    out.push(text(
        28.0,
        46.0,
        "Per-family replication rate by run (%)",
        15,
        INK,
        "start",
        Some(600),
        false,
    ));
    out.push(text(
        28.0,
        66.0,
        "Blue → green = low → high rate · families A–Z · \"–\" = not yet run",
        12,
        INK2,
        "start",
        None,
        false,
    ));
    for (j, run) in runs.iter().enumerate() {
        out.push(text(
            grid_x + j as f64 * (cell_w + gap) + cell_w / 2.0,
            94.0,
            &format!("Run {run}"),
            12,
            MUTED,
            "middle",
            None,
            false,
        ));
    }
    for (i, fam) in families.iter().enumerate() {
        let row_y = top + i as f64 * (cell_h + gap);
        // long names drop to 11px with a middle ellipsis (no font metrics here)
        let chars: Vec<char> = fam.chars().collect();
        let (name, size) = if chars.len() <= 20 {
            ((*fam).to_string(), 12)
        } else {
            let head: String = chars[..9].iter().collect();
            let tail: String = chars[chars.len() - 9..].iter().collect();
            (format!("{head}…{tail}"), 11)
        };
        out.push(text(
            gutter_x,
            row_y + 18.5,
            &name,
            size,
            INK2,
            "end",
            None,
            false,
        ));
        for (j, run) in runs.iter().enumerate() {
            let x = grid_x + j as f64 * (cell_w + gap);
            let center = x + cell_w / 2.0;
            match pivot[*fam].get(run) {
                None => {
                    // unscored cell: the card shows through, a muted dash marks the slot
                    out.push(text(
                        center,
                        row_y + 18.5,
                        "–",
                        11,
                        MUTED,
                        "middle",
                        Some(600),
                        false,
                    ));
                }
                Some(cell) => {
                    let fill = ramp_color(cell.rep, cell.par, cell.gt);
                    out.push(rect(x, row_y, cell_w, cell_h, fill, Some(2.0)));
                    out.push(text(
                        center,
                        row_y + 18.5,
                        &format!("{:.1}", cell.rate),
                        11,
                        cell_ink(fill),
                        "middle",
                        Some(600),
                        true,
                    ));
                }
            }
        }
    }
    // ramp key: six swatches flanked by the scale ends
    out.push(text(74.0, key_y + 9.0, "0", 11, MUTED, "end", None, true));
    for (k, color) in RAMP.iter().enumerate() {
        out.push(rect(
            80.0 + k as f64 * 22.0,
            key_y,
            20.0,
            10.0,
            color,
            Some(2.0),
        ));
    }
    out.push(text(
        80.0 + 6.0 * 22.0 + 4.0,
        key_y + 9.0,
        "100",
        11,
        MUTED,
        "start",
        None,
        true,
    ));
    out.push(text(
        64.0,
        footer_y,
        "Source: Scoreboard table · rate = (replicated + 0.5 × partial) / static GT",
        11,
        MUTED,
        "start",
        None,
        false,
    ));
    out.push("  </g>".to_string());
    out.push("</svg>".to_string());
    out.join("\n") + "\n"
}

/// Render the aggregate progression table, the figure's table twin
///
/// # Arguments
///
/// * `aggs` - The pooled per-run aggregates to tabulate
fn aggregate_table_md(aggs: &[RunAgg]) -> String {
    let mut lines = vec![
        "| Run | Families | Rate | Δ (pp) | Cost |".to_string(),
        "|-----|----------|------|--------|------|".to_string(),
    ];
    for a in aggs {
        // the delta column: suppressed, signed, or a flat zero
        let delta = match a.delta {
            None => "—".to_string(),
            Some(delta) if delta >= 0.05 => format!("+{delta:.1}"),
            Some(delta) if delta <= -0.05 => format!("−{:.1}", delta.abs()),
            Some(_) => "0.0".to_string(),
        };
        lines.push(format!(
            "| {} | {} | {:.1}% | {delta} | ${} |",
            a.run,
            a.n,
            a.rate,
            money(a.cost)
        ));
    }
    lines.join("\n")
}

/// Assemble the machine-owned markdown block: both figures and the table
///
/// # Arguments
///
/// * `aggs` - The pooled per-run aggregates
/// * `rows` - Every parsed Scoreboard row
fn figures_block(aggs: &[RunAgg], rows: &[Row]) -> String {
    let fig1_alt = progression_alt(aggs);
    // distinct family and run counts for the heatmap description
    let families: HashSet<&str> = rows.iter().map(|r| r.family.as_str()).collect();
    let runs: HashSet<i64> = rows.iter().map(|r| r.run).collect();
    let fig2_alt = format!(
        "Heatmap of replication rate for {} families across {} runs, rows alphabetical by family",
        families.len(),
        runs.len()
    );
    [
        "<p align=\"center\">".to_string(),
        format!("    <img width=\"800\" src=\"./figures/progression.svg\" alt=\"{fig1_alt}\">"),
        "</p>".to_string(),
        String::new(),
        aggregate_table_md(aggs),
        String::new(),
        "<p align=\"center\">".to_string(),
        format!(
            "    <img width=\"800\" src=\"./figures/per_family_heatmap.svg\" alt=\"{fig2_alt}\">"
        ),
        "</p>".to_string(),
    ]
    .join("\n")
}

/// Replace the machine-owned block between the eval-figures markers
///
/// # Arguments
///
/// * `md` - The full evaluation.md content
/// * `block` - The regenerated block to splice in
fn splice(md: &str, block: &str) -> Result<String, Error> {
    // every marker fault maps onto the same loud message
    let err =
        || Error::new("eval-figures markers missing, duplicated, or out of order in evaluation.md");
    // both markers must be present, in order, and appear exactly once each
    let b = md.find(BEGIN).ok_or_else(err)?;
    let e = md.find(END).ok_or_else(err)?;
    if e < b || md[b + 1..].find(BEGIN).is_some() || md[e + 1..].find(END).is_some() {
        return Err(err());
    }
    // rebuild the doc with the regenerated block between the markers
    Ok(format!("{}\n{block}\n{}", &md[..b + BEGIN.len()], &md[e..]))
}

/// Write a file only when its bytes change
///
/// # Arguments
///
/// * `path` - The artifact path to write
/// * `content` - The regenerated artifact content
fn write_artifact(path: &Path, content: &str) -> Result<(), Error> {
    // resolve the printable file name once
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // skip the write (and say so) when the bytes already match
    if path.exists() && fs::read(path)? == content.as_bytes() {
        println!("unchanged {name}");
        return Ok(());
    }
    fs::write(path, content)?;
    println!("written   {name}");
    Ok(())
}

/// Report whether an artifact is stale without writing anything
///
/// # Arguments
///
/// * `path` - The artifact path to compare
/// * `content` - The regenerated artifact content
fn check_artifact(path: &Path, content: &str) -> Result<bool, Error> {
    // resolve the printable file name once
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // an artifact is fresh only when it exists with exactly these bytes
    if path.exists() && fs::read(path)? == content.as_bytes() {
        println!("unchanged {name}");
        Ok(false)
    } else {
        println!("stale     {name}");
        Ok(true)
    }
}

/// Regenerate the evaluation figures from the Scoreboard table
///
/// # Arguments
///
/// * `cmd` - The figures command to run
pub fn handle(cmd: &BatchFigures) -> Result<(), Error> {
    // read the evaluation doc that drives everything
    let md = fs::read_to_string(&cmd.evaluation).map_err(|err| {
        Error::new(format!(
            "failed to read {}: {err}",
            cmd.evaluation.display()
        ))
    })?;
    // the figures live in the sibling figures/ directory
    let fig_dir = cmd.evaluation.with_file_name("figures");
    // parse the scoreboard and pool the per-run aggregates
    let rows = parse_scoreboard(&md)?;
    // recompute the trust-me Novel/CISA cells when a scoring dir is given
    if let Some(scoring) = &cmd.scoring {
        let (checked, skipped) = verify_scoring_cells(&rows, scoring)?;
        eprintln!(
            "scoring recompute: Novel/CISA cells verified for {checked} row(s), \
             {skipped} without a scoring JSON"
        );
    }
    let (aggs, warnings) = aggregate(&rows);
    // surface partial-run warnings on stderr exactly like the Python
    for warning in &warnings {
        eprintln!("{warning}");
    }
    if aggs.is_empty() {
        return Err(Error::new("no complete runs to aggregate"));
    }
    // print the aggregate table for an eyeball check against the prose
    println!("{}", aggregate_table_md(&aggs));
    // validate the markers before any artifact is written so failures stay atomic
    let spliced = splice(&md, &figures_block(&aggs, &rows))?;
    // render both figures
    let progression = emit_progression_svg(&aggs);
    let heatmap = emit_heatmap_svg(&rows);
    // in check mode compare every artifact, write nothing, and fail when stale
    if cmd.check {
        let mut stale = 0;
        stale += usize::from(check_artifact(
            &fig_dir.join("progression.svg"),
            &progression,
        )?);
        stale += usize::from(check_artifact(
            &fig_dir.join("per_family_heatmap.svg"),
            &heatmap,
        )?);
        stale += usize::from(check_artifact(&cmd.evaluation, &spliced)?);
        if stale > 0 {
            return Err(Error::new(format!(
                "{stale} artifact(s) stale; re-run without --check to regenerate"
            )));
        }
        return Ok(());
    }
    // make sure the figures directory exists before writing into it
    if let Err(err) = fs::create_dir(&fig_dir)
        && err.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(Error::from(err));
    }
    // write the three artifacts in the Python's order
    write_artifact(&fig_dir.join("progression.svg"), &progression)?;
    write_artifact(&fig_dir.join("per_family_heatmap.svg"), &heatmap)?;
    write_artifact(&cmd.evaluation, &spliced)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal evaluation doc around the given scoreboard rows
    ///
    /// # Arguments
    ///
    /// * `rows` - The newline-joined table rows to append under the header
    fn scoreboard_doc(rows: &str) -> String {
        // line 1 is the heading, 2 the header, 3 the separator, 4 the first row
        let header = "| Run | Family | Static GT | Replicated | Partial | Missed | Wrong | Rate | Tokens | Cost | Novel | CISA rate |";
        let sep = "|-----|--------|-----------|------------|---------|--------|-------|------|--------|------|-------|-----------|";
        format!("## Scoreboard\n{header}\n{sep}\n{rows}\n")
    }

    /// The synthetic fixture reproduces the Python-generated goldens byte for byte
    ///
    /// The goldens were produced by running api/docs/gen_eval_figures.py over
    /// testdata/synthetic_evaluation.md; the fixture covers the branches the
    /// real docs miss (partial run, every delta sign, the label collision
    /// flip, a >20-char family name, comma costs, and the 50%/100% bins).
    #[test]
    fn synthetic_fixture_matches_python_goldens() {
        let md = include_str!("testdata/synthetic_evaluation.md");
        let rows = parse_scoreboard(md).unwrap();
        let (aggs, warnings) = aggregate(&rows);
        // the partial run 6 is excluded with the Python's verbatim warning
        assert_eq!(
            warnings.join("\n") + "\n",
            include_str!("testdata/synthetic_stderr.golden")
        );
        // both svgs and the spliced doc must match the Python's bytes
        assert_eq!(
            emit_progression_svg(&aggs),
            include_str!("testdata/synthetic_progression.svg")
        );
        assert_eq!(
            emit_heatmap_svg(&rows),
            include_str!("testdata/synthetic_per_family_heatmap.svg")
        );
        assert_eq!(
            splice(md, &figures_block(&aggs, &rows)).unwrap(),
            include_str!("testdata/synthetic_evaluation_spliced.md")
        );
        // the Python's stdout was the table plus the three written lines
        let stdout = format!(
            "{}\nwritten   progression.svg\nwritten   per_family_heatmap.svg\nwritten   evaluation.md\n",
            aggregate_table_md(&aggs)
        );
        assert_eq!(stdout, include_str!("testdata/synthetic_stdout.golden"));
    }

    /// Novel/CISA cells recompute from scoring JSONs and stale cells fail
    #[test]
    fn verify_scoring_cells_recomputes_novel_and_cisa() {
        // build a throwaway scoring dir with one family's JSON
        let dir =
            std::env::temp_dir().join(format!("thorctl-figures-scoring-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 2 CISA statics (1 replicated + 1 partial = 75%), a dynamic CISA item
        // that must not count, and 2 verified novel findings
        let json = r#"{"result": {"items": [
            {"id": "GT-1", "scope": "static", "verdict": "replicated", "source": "CISA AA22-321A"},
            {"id": "GT-2", "scope": "static", "verdict": "partial", "source": "CISA AA22-321A"},
            {"id": "GT-3", "scope": "static", "verdict": "missed", "source": "vendor blog"},
            {"id": "GT-4", "scope": "dynamic", "verdict": "missed", "source": "CISA AA22-321A"}
        ], "novel": [{"claim": "a"}, {"claim": "b"}]}}"#;
        fs::write(dir.join("run7-fam_a.json"), json).unwrap();
        // a row builder over the published Novel/CISA cells
        let row = |novel: &str, cisa: &str| Row {
            run: 7,
            family: "Fam A".to_owned(),
            gt: 3,
            rep: 1,
            par: 1,
            missed: 1,
            wrong: 0,
            rate: 50.0,
            cost: 1.0,
            slug: Some("fam_a".to_owned()),
            novel_cell: novel.to_owned(),
            cisa_cell: cisa.to_owned(),
        };
        // matching cells verify; a row without a scoring JSON is skipped
        let mut missing = row("2", "75.0%");
        missing.slug = Some("fam_b".to_owned());
        let counts = verify_scoring_cells(&[row("2", "75.0%"), missing], &dir).unwrap();
        assert_eq!(counts, (1, 1));
        // a stale Novel cell fails naming the row and both counts
        let err = verify_scoring_cells(&[row("3", "75.0%")], &dir).unwrap_err();
        assert!(
            err.to_string().contains("Novel cell '3' != recomputed 2"),
            "err: {err}"
        );
        // a stale CISA rate fails the same way
        let err = verify_scoring_cells(&[row("2", "58.1%")], &dir).unwrap_err();
        assert!(
            err.to_string()
                .contains("CISA cell '58.1%' != recomputed 75.0%"),
            "err: {err}"
        );
        // an em-dash CISA cell is stale when CISA items exist
        let err = verify_scoring_cells(&[row("2", "—")], &dir).unwrap_err();
        assert!(
            err.to_string().contains("'—' but 2 CISA item(s)"),
            "err: {err}"
        );
    }

    /// `handle` writes the goldens, is idempotent, and honors --check
    #[test]
    fn handle_writes_and_checks_artifacts() {
        // build a throwaway docs tree under the system temp dir
        let dir = std::env::temp_dir().join(format!("thorctl-figures-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let eval = dir.join("evaluation.md");
        fs::write(&eval, include_str!("testdata/synthetic_evaluation.md")).unwrap();
        // a stale tree fails --check before anything is written
        let check = BatchFigures {
            evaluation: eval.clone(),
            check: true,
            scoring: None,
        };
        assert!(handle(&check).is_err());
        assert!(!dir.join("figures").exists());
        // a real run writes the Python goldens byte for byte
        let write = BatchFigures {
            evaluation: eval.clone(),
            check: false,
            scoring: None,
        };
        handle(&write).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("figures/progression.svg")).unwrap(),
            include_str!("testdata/synthetic_progression.svg")
        );
        assert_eq!(
            fs::read_to_string(dir.join("figures/per_family_heatmap.svg")).unwrap(),
            include_str!("testdata/synthetic_per_family_heatmap.svg")
        );
        assert_eq!(
            fs::read_to_string(&eval).unwrap(),
            include_str!("testdata/synthetic_evaluation_spliced.md")
        );
        // the regenerated tree passes --check and a rerun stays a fixed point
        handle(&check).unwrap();
        handle(&write).unwrap();
        assert_eq!(
            fs::read_to_string(&eval).unwrap(),
            include_str!("testdata/synthetic_evaluation_spliced.md")
        );
        // clean up the throwaway tree
        fs::remove_dir_all(&dir).unwrap();
    }

    /// `num` strips trailing zeros then the bare point
    #[test]
    fn num_strips_trailing_zeros() {
        assert_eq!(num(0.0), "0");
        assert_eq!(num(580.0), "580");
        assert_eq!(num(110.0), "110");
        assert_eq!(num(0.5), "0.5");
        assert_eq!(num(1.2), "1.2");
        assert_eq!(num(96.33333333333333), "96.33");
        assert_eq!(num(2.675), "2.67");
    }

    /// `round1` and `round_ties_even` match Python's banker rounding on edges
    #[test]
    fn rounding_matches_python() {
        // round(x, 1) edges: ties go to even on the exact binary value
        assert_eq!(round1(0.05), 0.1);
        assert_eq!(round1(0.15), 0.1);
        assert_eq!(round1(0.25), 0.2);
        assert_eq!(round1(0.35), 0.3);
        assert_eq!(round1(2.675), 2.7);
        assert_eq!(round1(55.849999999999994), 55.8);
        // round(x) edges: ties to even, never half away from zero
        assert_eq!(0.5f64.round_ties_even(), 0.0);
        assert_eq!(1.5f64.round_ties_even(), 2.0);
        assert_eq!(2.5f64.round_ties_even(), 2.0);
        assert_eq!(250.5f64.round_ties_even(), 250.0);
        assert_eq!(450.75f64.round_ties_even(), 451.0);
    }

    /// `thousands` and `money` group digits like Python's format specs
    #[test]
    fn thousands_and_money_group_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(2135), "2,135");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(money(0.0), "0.00");
        assert_eq!(money(2134.56), "2,134.56");
        assert_eq!(money(7436.01), "7,436.01");
        // the fixed conversion rounds before grouping, like Python
        assert_eq!(money(999.995), "1,000.00");
        assert_eq!(money(-1234.5), "-1,234.50");
    }

    /// `cost_scale` picks candidate steps and falls back to doubling
    #[test]
    fn cost_scale_picks_round_steps() {
        assert_eq!(cost_scale(100.0), (200.0, 100));
        assert_eq!(cost_scale(580.30), (800.0, 200));
        assert_eq!(cost_scale(2134.56), (2500.0, 500));
        assert_eq!(cost_scale(7436.01), (10000.0, 2000));
        // beyond the candidates the largest step doubles until it fits
        assert_eq!(cost_scale(20000.0), (24000.0, 4000));
        // an all-zero cost column still gets a nonzero scale
        assert_eq!(cost_scale(0.0), (100.0, 100));
    }

    /// `ramp_color` bins boundary rates deterministically in integer math
    #[test]
    fn ramp_color_bins_boundaries() {
        // 0% sits in the lowest bin
        assert_eq!(ramp_color(0, 0, 10), RAMP[0]);
        // 49.9% stays below the midpoint bin
        assert_eq!(ramp_color(499, 0, 1000), RAMP[2]);
        // an exact 50.0% lands in the upper bin
        assert_eq!(ramp_color(1, 0, 2), RAMP[3]);
        assert_eq!(ramp_color(5, 0, 10), RAMP[3]);
        // 100% clamps onto the top bin
        assert_eq!(ramp_color(2, 0, 2), RAMP[5]);
    }

    /// `esc` escapes the four specials with & first
    #[test]
    fn esc_escapes_in_order() {
        assert_eq!(esc("a&b<c>d\"e"), "a&amp;b&lt;c&gt;d&quot;e");
        // escaping & first double-escapes pre-escaped text like the Python
        assert_eq!(esc("&lt;"), "&amp;lt;");
    }

    /// A missing heading or altered pinned header fails loudly
    #[test]
    fn parse_rejects_missing_or_bad_header() {
        // no scoreboard heading at all
        let err = parse_scoreboard("# nothing here\n").unwrap_err();
        assert_eq!(
            err.msg().unwrap(),
            "no '## Scoreboard' heading in evaluation.md"
        );
        // a heading with a header whose columns changed
        let err =
            parse_scoreboard("## Scoreboard\n| Run | Family | Static GT |\n|---|\n").unwrap_err();
        assert_eq!(
            err.msg().unwrap(),
            "pinned Scoreboard header not found (columns changed?)"
        );
    }

    /// A row with the wrong cell count names its line
    #[test]
    fn parse_rejects_wrong_cell_count() {
        // this row drops the CISA rate cell, leaving 11
        let md = scoreboard_doc("| 1 | [A](./a.md) | 10 | 5 | 0 | 5 | 0 | 50.0% | t | $1.00 | — |");
        let err = parse_scoreboard(&md).unwrap_err();
        assert_eq!(
            err.msg().unwrap(),
            "evaluation.md:4: expected 12 cells, found 11"
        );
    }

    /// A duplicated (run, family) pair names its line
    #[test]
    fn parse_rejects_duplicate_rows() {
        // the same family scored twice in the same run
        let row = "| 1 | [A](./a.md) | 10 | 5 | 0 | 5 | 0 | 50.0% | t | $1.00 | — | — |";
        let md = scoreboard_doc(&format!("{row}\n{row}"));
        let err = parse_scoreboard(&md).unwrap_err();
        assert_eq!(
            err.msg().unwrap(),
            "evaluation.md:5: duplicate row for run 1 / A"
        );
    }

    /// A printed rate that disagrees with the counts names its line
    #[test]
    fn parse_rejects_rate_mismatch() {
        // 5 of 10 replicated cannot print as 99.0%
        let md =
            scoreboard_doc("| 1 | [A](./a.md) | 10 | 5 | 0 | 5 | 0 | 99.0% | t | $1.00 | — | — |");
        let err = parse_scoreboard(&md).unwrap_err();
        assert_eq!(
            err.msg().unwrap(),
            "evaluation.md:4: rate cross-check failed: printed 99.0%, computed 50.0%"
        );
    }

    /// Marker problems are rejected before any artifact write
    #[test]
    fn splice_rejects_bad_markers() {
        // missing markers entirely
        assert!(splice("no markers", "x").is_err());
        // end before begin
        assert!(
            splice(
                "<!-- eval-figures:end -->\n<!-- eval-figures:begin -->",
                "x"
            )
            .is_err()
        );
        // a duplicated begin marker
        let doubled =
            "<!-- eval-figures:begin -->\n<!-- eval-figures:begin -->\n<!-- eval-figures:end -->";
        assert!(splice(doubled, "x").is_err());
    }
}
