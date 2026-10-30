//! The telemetry export flow: scan, fold, redact, preview, confirm, write.
//!
//! The interactive path walks five screens -- session picker, tier choice,
//! redaction review, preview, write -- and honors one contract throughout:
//! the preview *is* the bundle. The exact bytes the preview renders are the
//! bytes the writer writes, and any review change re-renders them before
//! confirmation. The headless `--yes` path applies the same fold and
//! redaction, writes without a terminal, and prints one stable
//! machine-parseable summary line on stderr (the `usage:` tail's sibling).

use chrono::{DateTime, SubsecRound, Utc};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use kanal::{AsyncReceiver, AsyncSender};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thorium::{CtlConf, Error};
use thorpal::{LibraryRedactor, SessionStore, parse_token_file};

use crate::args::Args;
use crate::args::ai::TelemetryExport;
use crate::handlers::ai::chat::{ViewOutcome, fmt_duration, with_thousands};
use crate::handlers::ai::tui;
use crate::handlers::ai::usage::model::SessionSummary;
use crate::handlers::ai::usage::scan::{PricingCtx, ScanUpdate, run_scanner, scan_once};

use super::bundle::{
    BUNDLE_VERSION, Bundle, DetectorTally, SIGNATURE_RECIPE, SessionTelemetry, SignatureCluster,
};
use super::fold::{assemble_session, is_native_technique, read_forum};
use super::shell::{Popup, TelemetryTheme, signature_popup};

/// The reserved detector key dropped non-native technique ids fold under
const OTHER_TECHNIQUES: &str = "other";

/// The deny-token placeholder shape the library redactor emits
///
/// The redactor's `fam-<4 hex of sha256(token)>` placeholders are fine for
/// model-visible text, but a bundle's receiver could dictionary-match that
/// fingerprint back to a candidate token list -- so exports flatten every
/// placeholder to one constant marker that carries nothing.
static FAM_PLACEHOLDER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    // unanchored on purpose: the redactor splices placeholders into the
    // middle of identifiers (`MAL_fam-ab12_Loader`), where a word boundary
    // would never match and the reversible fingerprint would ship
    regex::Regex::new(r"fam-[0-9a-f]{4}").expect("a valid placeholder regex")
});

/// The flat marker exported strings carry where a deny token matched
const DENIED: &str = "[denied]";

/// Redacts one exported string: deny tokens out, fingerprints flattened
///
/// # Arguments
///
/// * `redactor` - The deny-token redactor, when one was built
/// * `text` - The string to redact
fn export_redact(redactor: Option<&LibraryRedactor>, text: &str) -> String {
    // without a redactor the structural denylist is the whole posture
    let Some(redactor) = redactor else {
        return text.to_owned();
    };
    // scrub the tokens, then flatten the reversible placeholder fingerprints
    let scrubbed = redactor.redact_text(text);
    FAM_PLACEHOLDER.replace_all(&scrubbed, DENIED).into_owned()
}

/// Run a telemetry export, interactive or headless
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `export` - The parsed `telemetry export` command
pub async fn handle(args: &Args, export: &TelemetryExport) -> Result<(), Error> {
    // resolve the sessions root: the explicit flag outranks the default
    let root = match &export.sessions_root {
        Some(root) => root.clone(),
        None => SessionStore::default_root().map_err(|err| Error::new(format!("{err:#}")))?,
    };
    // pricing is best-effort like `usage`: bundles carry raw token counts
    // only, so a missing config costs nothing here
    let pricing = PricingCtx::from_conf(CtlConf::from_path(&args.config).ok());
    // load the deny tokens up front so a bad file fails before any screen;
    // the list itself is never persisted anywhere
    let redactor = build_redactor(export.redact_tokens.as_deref())?;
    // route to the headless writer or the interactive wizard
    if export.yes {
        headless(&root, &pricing, export, redactor).await
    } else {
        // route tracing to the shared log file before taking the screen
        tui::init_tracing_log(None);
        // take the screen only after everything that can fail has failed
        let mut terminal = tui::init_terminal()?;
        // build and run the wizard, capturing the result so teardown runs
        let mut app = ExportApp::new(root, pricing, export, redactor);
        let result = app.start(&mut terminal).await;
        // give the screen back before surfacing any error
        tui::restore_terminal();
        result?;
        Ok(())
    }
}

/// Builds the deny-token redactor, when a token file was given
///
/// # Arguments
///
/// * `path` - The deny-token file to load, if any
fn build_redactor(path: Option<&Path>) -> Result<Option<Arc<LibraryRedactor>>, Error> {
    // no file means no redaction beyond the structural denylist
    let Some(path) = path else {
        return Ok(None);
    };
    // read and parse the token list exactly like the chat flag does
    let contents = std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
    let tokens = parse_token_file(&contents);
    // an all-comments file redacts nothing; the builder returns None then
    Ok(LibraryRedactor::new(false, tokens, Vec::new()))
}

/// Runs the headless export: same fold and redaction, no terminal
///
/// # Arguments
///
/// * `root` - The sessions root to scan
/// * `pricing` - The config's best-effort pricing context
/// * `export` - The parsed `telemetry export` command
/// * `redactor` - The deny-token redactor, when one was built
async fn headless(
    root: &Path,
    pricing: &PricingCtx,
    export: &TelemetryExport,
    redactor: Option<Arc<LibraryRedactor>>,
) -> Result<(), Error> {
    // fold every session once, telemetry counters included
    let summaries = scan_once(root, pricing).await;
    // narrow to the requested sessions, failing on names that do not exist
    let selected = select_summaries(summaries, &export.session)?;
    if selected.is_empty() {
        return Err(Error::new(format!(
            "no sessions to export under {}",
            root.display()
        )));
    }
    // join each session's forum pass into its bundle row
    let rows = assemble_rows(root, &selected).await;
    // apply the tier and redaction, then render the exact output bytes
    let bundle = finalize(
        &rows,
        export.level,
        redactor.as_deref(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        &BTreeSet::new(),
        export_stamp(),
    );
    let bytes = bundle.to_bytes()?;
    // write the bundle and print the stable summary tail
    let out = export
        .out
        .clone()
        .unwrap_or_else(|| default_out_name(Utc::now()));
    Bundle::write_bytes(&bytes, &out)?;
    eprintln!("{}", summary_line(&bundle, &bytes, &out));
    Ok(())
}

/// Narrows scanned summaries to the requested session names
///
/// With no requested names every summary exports; with names, each must
/// exist so a typo can never silently export the wrong set.
///
/// # Arguments
///
/// * `summaries` - Every scanned session summary
/// * `requested` - The `--session` names, possibly empty
fn select_summaries(
    summaries: Vec<SessionSummary>,
    requested: &[String],
) -> Result<Vec<SessionSummary>, Error> {
    // no explicit names means the whole store exports
    if requested.is_empty() {
        return Ok(summaries);
    }
    // index what the scan found and demand every requested name
    let mut by_name: BTreeMap<String, SessionSummary> = summaries
        .into_iter()
        .map(|summary| (summary.name.clone(), summary))
        .collect();
    let mut selected = Vec::with_capacity(requested.len());
    let mut missing = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for name in requested {
        // the same name twice is one selection, never a missing session
        if !seen.insert(name.as_str()) {
            continue;
        }
        match by_name.remove(name) {
            Some(summary) => selected.push(summary),
            None => missing.push(name.as_str()),
        }
    }
    if !missing.is_empty() {
        return Err(Error::new(format!(
            "no such session(s): {}",
            missing.join(", ")
        )));
    }
    Ok(selected)
}

/// Joins each summary with its forum pass into a bundle row
///
/// # Arguments
///
/// * `root` - The sessions root the summaries were scanned from
/// * `selected` - The summaries to assemble
async fn assemble_rows(root: &Path, selected: &[SessionSummary]) -> Vec<SessionTelemetry> {
    let mut rows = Vec::with_capacity(selected.len());
    for summary in selected {
        // the forum pass reads forum.json directly and never opens a store
        let outcomes = read_forum(&root.join(&summary.name)).await;
        rows.push(assemble_session(summary, outcomes));
    }
    rows
}

/// One technique id's export form: dropped, kept, or redacted
///
/// Native ATT&CK ids are a closed vocabulary and pass as-is; everything else
/// is minted from analyst-influenced inputs and gets the deny-token redactor;
/// a reviewed-out id folds under the reserved `other` key so totals stay
/// honest wherever the id appeared.
///
/// # Arguments
///
/// * `id` - The technique id to map
/// * `redactor` - The deny-token redactor, when one was built
/// * `dropped_ids` - The technique ids the review dropped
fn export_id(
    id: &str,
    redactor: Option<&LibraryRedactor>,
    dropped_ids: &BTreeSet<String>,
) -> String {
    if dropped_ids.contains(id) {
        OTHER_TECHNIQUES.to_owned()
    } else if is_native_technique(id) {
        id.to_owned()
    } else {
        export_redact(redactor, id)
    }
}

/// Applies the tier, drops, and redaction, producing the final bundle
///
/// Every string that can carry analyst-influenced text -- level-1 signatures,
/// level-1 forum entry titles, and non-native technique ids -- passes through
/// the deny-token redactor; dropped signatures vanish, dropped titles blank
/// to `None` (the entry row survives), and dropped technique ids fold their
/// tallies under the reserved `other` key so totals stay honest. Level 0
/// clears every signature and every title outright.
///
/// # Arguments
///
/// * `rows` - The assembled per-session rows
/// * `level` - The chosen bundle level
/// * `redactor` - The deny-token redactor, when one was built
/// * `dropped_sigs` - The `(tool, signature)` clusters the review dropped
/// * `dropped_ids` - The technique ids the review dropped
/// * `dropped_titles` - The (redacted) entry titles the review dropped
/// * `created_at` - The export stamp (a parameter so two finalizes made for
///   a byte-delta comparison can share one timestamp)
#[allow(clippy::too_many_arguments)]
fn finalize(
    rows: &[SessionTelemetry],
    level: u8,
    redactor: Option<&LibraryRedactor>,
    dropped_sigs: &BTreeSet<(String, String)>,
    dropped_ids: &BTreeSet<String>,
    dropped_titles: &BTreeSet<String>,
    created_at: DateTime<Utc>,
) -> Bundle {
    // redact helper: deny tokens out, placeholder fingerprints flattened
    let redact = |text: &str| export_redact(redactor, text);
    // order the rows by session id so the wizard and the headless path
    // render byte-identical bundles for one selection (created_at aside)
    let mut ordered: Vec<&SessionTelemetry> = rows.iter().collect();
    ordered.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let mut sessions = Vec::with_capacity(ordered.len());
    for row in ordered {
        let mut row = row.clone();
        // the tier gates signatures entirely; drops and redaction apply at 1
        if level == 0 {
            row.signatures.clear();
        } else {
            // redact first (the review screen showed the redacted strings,
            // so drops key them), then drop, then re-merge the rest
            // (redaction can collapse two signatures into one string)
            let mut merged: BTreeMap<(String, String), SignatureCluster> = BTreeMap::new();
            for cluster in row.signatures.drain(..) {
                let sig = redact(&cluster.sig);
                let key = (cluster.tool.clone(), sig.clone());
                if dropped_sigs.contains(&key) {
                    continue;
                }
                let entry = merged
                    .entry((cluster.tool.clone(), sig.clone()))
                    .or_insert_with(|| SignatureCluster {
                        tool: cluster.tool.clone(),
                        sig,
                        count: 0,
                        looped: false,
                    });
                entry.count += cluster.count;
                entry.looped |= cluster.looped;
            }
            let mut signatures: Vec<SignatureCluster> = merged.into_values().collect();
            // keep the heaviest-first order previews read naturally
            signatures.sort_by(|a, b| {
                b.count
                    .cmp(&a.count)
                    .then_with(|| a.tool.cmp(&b.tool))
                    .then_with(|| a.sig.cmp(&b.sig))
            });
            row.signatures = signatures;
        }
        // forum entry prose is the level-1 opt-in: level 0 clears every
        // title; level 1 redacts each and honors the review's drops
        for entry in &mut row.forum.entries {
            entry.title = match entry.title.take() {
                Some(title) if level == 1 => {
                    let redacted = redact(&title);
                    // a dropped title blanks; the entry row itself survives
                    (!dropped_titles.contains(&redacted)).then_some(redacted)
                }
                _ => None,
            };
            // entry technique ids get the same scrutiny as the tally keys
            let mut techniques: Vec<String> = entry
                .techniques
                .iter()
                .map(|id| export_id(id, redactor, dropped_ids))
                .collect();
            techniques.sort();
            techniques.dedup();
            entry.techniques = techniques;
        }
        // technique ids get the same scrutiny at both levels: native ids pass
        // as-is, non-native ids redact, dropped ids fold under `other` --
        // tallies, histograms, and per-lead rows alike
        let mut detectors: BTreeMap<String, DetectorTally> = BTreeMap::new();
        for (id, tally) in &row.detectors {
            let key = export_id(id, redactor, dropped_ids);
            let entry = detectors.entry(key).or_default();
            entry.confirmed += tally.confirmed;
            entry.disputed += tally.disputed;
            entry.neutral += tally.neutral;
            entry.ignored += tally.ignored;
            entry.leads += tally.leads;
            for (bucket, count) in &tally.confidence {
                *entry.confidence.entry(bucket.clone()).or_default() += count;
            }
        }
        row.detectors = detectors;
        for lead in &mut row.detector_leads {
            lead.technique = export_id(&lead.technique, redactor, dropped_ids);
        }
        // the roster's technique ids get the same scrutiny as the tally keys
        // (its names are shape-gated to registry identifiers at fold time)
        for tally in row.detectors_run.values_mut() {
            tally.technique = export_id(&tally.technique, redactor, dropped_ids);
        }
        sessions.push(row);
    }
    Bundle {
        bundle_version: BUNDLE_VERSION,
        level,
        created_at: Some(created_at),
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        signature_recipe: SIGNATURE_RECIPE,
        sessions,
    }
}

/// The creation stamp a finalized bundle carries, truncated to whole seconds
///
/// Whole seconds keep every render of one selection the same byte length
/// (chrono's serializer varies its subsecond digits with the value), so the
/// tier screen's advertised sizes always match the previewed and written
/// bytes even though they finalize at different moments.
fn export_stamp() -> DateTime<Utc> {
    // chop the subseconds; bundle stamps never need them
    Utc::now().trunc_subsecs(0)
}

/// Both tiers' exact byte sizes, rendered with one shared timestamp
#[derive(Debug, Clone, Copy, Default)]
struct TierSizes {
    /// Level 0's exact byte size
    level0: usize,
    /// Level 1's exact byte size
    level1: usize,
    /// How many signature clusters level 1 ships (after drops)
    signatures: usize,
    /// How many entry titles level 1 ships (after drops)
    titles: usize,
}

/// The default bundle file name, stamped to the second
///
/// # Arguments
///
/// * `now` - The export's wall-clock time
fn default_out_name(now: DateTime<Utc>) -> PathBuf {
    // a sortable UTC stamp keeps repeated exports from colliding
    PathBuf::from(format!(
        "thorpal-telemetry-{}.json",
        now.format("%Y%m%d-%H%M%S")
    ))
}

/// The stable machine-parseable stderr tail a headless export prints
///
/// The format is a contract like the headless `usage:` line: batch tooling
/// may parse it, so changes here need the same care. The digest covers the
/// exact bytes written (before any gzip framing), so an analyst can certify
/// what left the machine.
///
/// # Arguments
///
/// * `bundle` - The finalized bundle
/// * `bytes` - The exact rendered bytes that were written
/// * `out` - The path the bundle was written to
fn summary_line(bundle: &Bundle, bytes: &[u8], out: &Path) -> String {
    // count the signatures across every session for the tail
    let signatures: usize = bundle
        .sessions
        .iter()
        .map(|session| session.signatures.len())
        .sum();
    format!(
        "telemetry: sessions={} level={} signatures={} bytes={} sha256={:x} out={}",
        bundle.sessions.len(),
        bundle.level,
        signatures,
        bytes.len(),
        Sha256::digest(bytes),
        out.display()
    )
}

/// The wizard's screens in walk order with their crumb labels
///
/// The single source of screen order: the crumb header, the per-screen help
/// index, and the labels all derive from this table.
const WALK: [(Screen, &str); 5] = [
    (Screen::Picker, "sessions"),
    (Screen::Tier, "tier"),
    (Screen::Review, "review"),
    (Screen::Preview, "preview"),
    (Screen::Done, "write"),
];

/// A wizard screen's position in the walk order
///
/// # Arguments
///
/// * `screen` - The screen to position
fn screen_index(screen: Screen) -> usize {
    // the WALK table is the single source of screen order
    WALK.iter()
        .position(|(walk, _)| *walk == screen)
        .unwrap_or(0)
}

/// A wizard screen's human-facing label
///
/// # Arguments
///
/// * `screen` - The screen to label
fn screen_label(screen: Screen) -> &'static str {
    // the same labels the step-crumb header renders
    WALK[screen_index(screen)].1
}

/// One event the export wizard's loop handles
enum ExportEvent {
    /// A terminal event (keyboard, mouse, resize, etc.)
    Terminal(Event),
    /// An update from the read-only session scanner
    Scan(ScanUpdate),
    /// Quit the TUI gracefully (e.g. a SIGTERM asked the process to stop)
    Quit,
    /// Redraw the tui
    Redraw,
}

/// The wizard's screens, in walk order
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// The multi-select session picker
    Picker,
    /// The level 0 / level 1 tier choice
    Tier,
    /// The per-signature and per-technique-id redaction review
    Review,
    /// The exact-bytes preview and confirmation
    Preview,
    /// The written-bundle summary
    Done,
}

/// One reviewable item on the redaction screen
#[derive(Debug, Clone)]
enum ReviewItem {
    /// A level-1 signature cluster, droppable individually
    Signature {
        /// The tool the cluster belongs to
        tool: String,
        /// The normalized (and redacted) signature text
        sig: String,
        /// The cluster's count across the selected sessions
        count: u64,
        /// Whether any session flagged the cluster as a stuck loop
        looped: bool,
    },
    /// A non-native technique id, droppable into the `other` bucket
    TechniqueId {
        /// The technique id as parsed from the forum
        id: String,
    },
    /// A forum entry's title (level 1 only), droppable individually
    EntryTitle {
        /// The redacted title text that would ship
        title: String,
        /// How many entries across the selection carry it
        count: u64,
    },
}

/// The export wizard's application state
struct ExportApp {
    /// The sender feeding the app's own event loop
    event_tx: AsyncSender<ExportEvent>,
    /// The receiver the app's event loop drains
    event_rx: AsyncReceiver<ExportEvent>,
    /// The sessions root being scanned
    root: PathBuf,
    /// The config's best-effort pricing context
    pricing: PricingCtx,
    /// The scanner's rescan receiver, taken by `start`
    rescan_rx: Option<tokio::sync::mpsc::Receiver<()>>,
    /// The channel an `r` press nudges the scanner over
    rescan_tx: tokio::sync::mpsc::Sender<()>,
    /// The deny-token redactor, when one was built
    redactor: Option<Arc<LibraryRedactor>>,
    /// The chosen bundle level
    level: u8,
    /// The `--session` names to preselect once scanned
    requested: Vec<String>,
    /// Where the bundle will be written
    out: PathBuf,
    /// Every scanned session's summary, keyed by name
    sessions: BTreeMap<String, SessionSummary>,
    /// The initial scan's progress, until its first pass settles
    progress: Option<(usize, usize)>,
    /// The screen currently showing
    screen: Screen,
    /// The picker's cursor index into the sorted session list
    cursor: usize,
    /// The picker's selected session names
    selected: BTreeSet<String>,
    /// The assembled bundle rows for the selected sessions
    rows: Vec<SessionTelemetry>,
    /// The review screen's cursor index
    review_cursor: usize,
    /// The review screen's items, rebuilt whenever the rows change
    review_items: Vec<ReviewItem>,
    /// The `(tool, signature)` clusters the review dropped
    dropped_sigs: BTreeSet<(String, String)>,
    /// The technique ids the review dropped
    dropped_ids: BTreeSet<String>,
    /// The (redacted) forum entry titles the review dropped
    dropped_titles: BTreeSet<String>,
    /// The finalized bundle behind the preview, kept for the summary line
    bundle: Option<Bundle>,
    /// The exact bytes the preview renders and the writer writes
    bytes: Vec<u8>,
    /// Both tiers' byte sizes under the current drops, when computed
    tier_sizes: Option<TierSizes>,
    /// Whether the review screen was seen at level 1 (the unreviewed warning)
    reviewed_at_level1: bool,
    /// The preview's first visible line
    scroll: usize,
    /// The open pop-out window, rendered over the screen and keyed first
    popup: Option<Popup>,
    /// The shell's shared styles
    theme: TelemetryTheme,
    /// The written bundle's summary line, once written
    done_line: Option<String>,
    /// The last error, shown in the footer until the next action
    error: Option<String>,
    /// Set once the user asked to quit
    should_quit: bool,
}

impl ExportApp {
    /// Creates the wizard over a sessions root
    ///
    /// # Arguments
    ///
    /// * `root` - The sessions root to scan
    /// * `pricing` - The config's best-effort pricing context
    /// * `export` - The parsed `telemetry export` command
    /// * `redactor` - The deny-token redactor, when one was built
    fn new(
        root: PathBuf,
        pricing: PricingCtx,
        export: &TelemetryExport,
        redactor: Option<Arc<LibraryRedactor>>,
    ) -> Self {
        // the app's own event channel, shared with every background task
        let (event_tx, event_rx) = kanal::unbounded_async();
        // the nudge channel the refresh key pokes the scanner over
        let (rescan_tx, rescan_rx) = tokio::sync::mpsc::channel(1);
        ExportApp {
            event_tx,
            event_rx,
            root,
            pricing,
            rescan_rx: Some(rescan_rx),
            rescan_tx,
            redactor,
            level: export.level,
            requested: export.session.clone(),
            out: export
                .out
                .clone()
                .unwrap_or_else(|| default_out_name(Utc::now())),
            sessions: BTreeMap::new(),
            progress: Some((0, 0)),
            screen: Screen::Picker,
            cursor: 0,
            selected: BTreeSet::new(),
            rows: Vec::new(),
            review_cursor: 0,
            review_items: Vec::new(),
            dropped_sigs: BTreeSet::new(),
            dropped_ids: BTreeSet::new(),
            dropped_titles: BTreeSet::new(),
            bundle: None,
            bytes: Vec::new(),
            tier_sizes: None,
            reviewed_at_level1: false,
            scroll: 0,
            popup: None,
            theme: TelemetryTheme::default(),
            done_line: None,
            error: None,
            should_quit: false,
        }
    }

    /// Runs the wizard's event loop until the user quits
    ///
    /// # Arguments
    ///
    /// * `terminal` - The terminal to render frames into
    async fn start(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        // spawn the shared terminal and SIGTERM forwarders
        tui::spawn_forwarders(&self.event_tx, ExportEvent::Terminal, || ExportEvent::Quit);
        // spawn the read-only telemetry scanner feeding the picker; the
        // receiver was created with the app, so a second start has none
        let Some(rescan_rx) = self.rescan_rx.take() else {
            return Ok(());
        };
        tokio::spawn(run_scanner(
            self.root.clone(),
            self.pricing.clone(),
            self.event_tx.clone(),
            rescan_rx,
            // the exporter's folds gather the telemetry counters
            true,
            ExportEvent::Scan,
        ));
        // spawn the slow ticker keeping relative times fresh
        tokio::spawn(tui::redraw_ticker(self.event_tx.clone(), || {
            ExportEvent::Redraw
        }));
        // draw an initial frame until we get an event to handle
        terminal.draw(|frame| self.render(frame))?;
        // handle events one at a time; a wizard's traffic never needs more
        while let Ok(event) = self.event_rx.recv().await {
            self.handle_event(event).await;
            if self.should_quit {
                break;
            }
            terminal.draw(|frame| self.render(frame))?;
        }
        Ok(())
    }

    /// Handles one event off the loop
    ///
    /// # Arguments
    ///
    /// * `event` - The event to handle
    async fn handle_event(&mut self, event: ExportEvent) {
        match event {
            // keys route by the showing screen
            ExportEvent::Terminal(Event::Key(key)) => {
                // ignore key releases/repeat chatter on terminals that send it
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key).await;
                }
            }
            ExportEvent::Terminal(Event::Mouse(mouse)) => match mouse.kind {
                // the wheel moves whatever list or preview is showing
                MouseEventKind::ScrollUp => self.handle_scroll(true),
                MouseEventKind::ScrollDown => self.handle_scroll(false),
                _ => {}
            },
            // resizes and pastes just fall through to the coming redraw
            ExportEvent::Terminal(_) => {}
            // fold a scanner update into the picker's rows
            ExportEvent::Scan(update) => self.handle_scan(update),
            // quit gracefully
            ExportEvent::Quit => self.should_quit = true,
            // a redraw happens after every handled event anyway
            ExportEvent::Redraw => {}
        }
    }

    /// Folds one scanner update into the picker's state
    ///
    /// # Arguments
    ///
    /// * `update` - The scanner update to fold
    fn handle_scan(&mut self, update: ScanUpdate) {
        match update {
            // initial-pass progress for the footer
            ScanUpdate::Progress { done, total } => {
                if self.progress.is_some() {
                    self.progress = Some((done, total));
                }
            }
            // a fresh summary replaces (or creates) its session's row
            ScanUpdate::Loaded(summary) => {
                // honor the --session preselection as names come in; with no
                // explicit names every scanned session starts selected
                let preselect = self.requested.is_empty() || self.requested.contains(&summary.name);
                if preselect && !self.sessions.contains_key(&summary.name) {
                    self.selected.insert(summary.name.clone());
                }
                self.sessions.insert(summary.name.clone(), *summary);
            }
            // a vanished session drops out entirely
            ScanUpdate::Removed(name) => {
                self.sessions.remove(&name);
                self.selected.remove(&name);
            }
            // the first settled pass clears the progress readout for good
            ScanUpdate::PassDone => self.progress = None,
        }
    }

    /// Handles a key press by the showing screen
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    async fn handle_key(&mut self, key: KeyEvent) {
        // any action clears a stale footer error
        self.error = None;
        // ctrl-c always quits, whatever is showing
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // an open pop-out owns the keyboard; Close drops it and never also
        // navigates the wizard
        if let Some(popup) = &mut self.popup {
            if let ViewOutcome::Close = popup.handle_key(key) {
                self.popup = None;
            }
            return;
        }
        // help opens on every screen; `h` is unbound in the wizard's screens
        if matches!(key.code, KeyCode::Char('?') | KeyCode::Char('h')) {
            // the per-screen page, index-aligned with the WALK order
            self.popup = Some(Popup::markdown(
                format!(" help — {} ", screen_label(self.screen)),
                super::help::export_help(screen_index(self.screen)),
            ));
            return;
        }
        match self.screen {
            Screen::Picker => self.picker_key(key).await,
            Screen::Tier => self.tier_key(key).await,
            Screen::Review => self.review_key(key),
            Screen::Preview => self.preview_key(key),
            Screen::Done => {
                // any key leaves the wizard once the bundle is written
                self.should_quit = true;
            }
        }
    }

    /// Handles a wheel scroll by the showing screen
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    fn handle_scroll(&mut self, up: bool) {
        // an open pop-out owns the wheel
        if let Some(popup) = &mut self.popup {
            popup.handle_scroll(up);
            return;
        }
        match self.screen {
            // lists treat the wheel as cursor movement
            Screen::Picker => self.move_cursor(up),
            Screen::Review => self.move_review_cursor(up),
            // the preview scrolls its byte window
            Screen::Preview => {
                if up {
                    self.scroll = self.scroll.saturating_sub(3);
                } else {
                    self.scroll = self.scroll.saturating_add(3);
                }
            }
            _ => {}
        }
    }

    /// The picker's sessions in display order (newest activity first)
    fn picker_order(&self) -> Vec<&SessionSummary> {
        let mut order: Vec<&SessionSummary> = self.sessions.values().collect();
        // newest activity first, name as the stable tiebreak
        order.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then_with(|| a.name.cmp(&b.name)));
        order
    }

    /// Moves the picker cursor, clamped to the list
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the cursor moves up
    fn move_cursor(&mut self, up: bool) {
        let len = self.sessions.len();
        if up {
            self.cursor = self.cursor.saturating_sub(1);
        } else {
            self.cursor = (self.cursor + 1).min(len.saturating_sub(1));
        }
    }

    /// Moves the review cursor, clamped to the list
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the cursor moves up
    fn move_review_cursor(&mut self, up: bool) {
        let len = self.review_items.len();
        if up {
            self.review_cursor = self.review_cursor.saturating_sub(1);
        } else {
            self.review_cursor = (self.review_cursor + 1).min(len.saturating_sub(1));
        }
    }

    /// Handles a key on the session picker
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    async fn picker_key(&mut self, key: KeyEvent) {
        match key.code {
            // the picker is the first screen, so escape quits
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(true),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(false),
            // space toggles the session under the cursor
            KeyCode::Char(' ') => {
                if let Some(summary) = self.picker_order().get(self.cursor) {
                    let name = summary.name.clone();
                    if !self.selected.remove(&name) {
                        self.selected.insert(name);
                    }
                }
            }
            // `a` toggles between everything and nothing selected
            KeyCode::Char('a') => {
                if self.selected.len() == self.sessions.len() {
                    self.selected.clear();
                } else {
                    self.selected = self.sessions.keys().cloned().collect();
                }
            }
            // `r` nudges the scanner for a fresh pass
            KeyCode::Char('r') => {
                let _ = self.rescan_tx.try_send(());
            }
            // enter assembles the selection and advances to the tier choice
            KeyCode::Enter => {
                if self.selected.is_empty() {
                    self.error = Some("select at least one session (space toggles)".to_owned());
                    return;
                }
                self.assemble_selection().await;
                self.screen = Screen::Tier;
            }
            _ => {}
        }
    }

    /// Assembles the selected sessions' bundle rows and review items
    async fn assemble_selection(&mut self) {
        // join each selected summary with its forum pass, picker order
        let selected: Vec<SessionSummary> = self
            .picker_order()
            .into_iter()
            .filter(|summary| self.selected.contains(&summary.name))
            .cloned()
            .collect();
        self.rows = assemble_rows(&self.root, &selected).await;
        self.invalidate_tier_sizes();
        // new rows mean unseen prose: the level-1 review warning re-arms
        self.reviewed_at_level1 = false;
        self.rebuild_review_items();
    }

    /// Rebuilds the review screen's items from the assembled rows
    ///
    /// Items carry the redacted strings (the exact text that would ship), so
    /// drops key what the analyst actually reviewed; signatures appear only
    /// at level 1, matching what the chosen tier ships.
    fn rebuild_review_items(&mut self) {
        // merge signature clusters across sessions so each lists once
        let mut sigs: BTreeMap<(String, String), (u64, bool)> = BTreeMap::new();
        let mut ids: BTreeSet<String> = BTreeSet::new();
        let mut titles: BTreeMap<String, u64> = BTreeMap::new();
        for row in &self.rows {
            // signatures ship (and review) only at level 1
            if self.level == 1 {
                for cluster in &row.signatures {
                    let sig = export_redact(self.redactor.as_deref(), &cluster.sig);
                    let entry = sigs.entry((cluster.tool.clone(), sig)).or_default();
                    entry.0 += cluster.count;
                    entry.1 |= cluster.looped;
                }
            }
            // only non-native ids are reviewable; native ids are closed vocab
            for id in row.detectors.keys() {
                if !is_native_technique(id) {
                    ids.insert(id.clone());
                }
            }
            for entry in &row.forum.entries {
                for id in &entry.techniques {
                    if !is_native_technique(id) {
                        ids.insert(id.clone());
                    }
                }
                // entry titles ship (and review) only at level 1, and the
                // review shows the redacted text drops actually key
                if self.level == 1
                    && let Some(title) = &entry.title
                {
                    let redacted = export_redact(self.redactor.as_deref(), title);
                    *titles.entry(redacted).or_default() += 1;
                }
            }
        }
        // heaviest signatures first, then the technique ids
        let mut items: Vec<ReviewItem> = sigs
            .into_iter()
            .map(|((tool, sig), (count, looped))| ReviewItem::Signature {
                tool,
                sig,
                count,
                looped,
            })
            .collect();
        items.sort_by(|a, b| match (a, b) {
            (
                ReviewItem::Signature { count: a_count, .. },
                ReviewItem::Signature { count: b_count, .. },
            ) => b_count.cmp(a_count),
            _ => std::cmp::Ordering::Equal,
        });
        items.extend(ids.into_iter().map(|id| ReviewItem::TechniqueId { id }));
        items.extend(
            titles
                .into_iter()
                .map(|(title, count)| ReviewItem::EntryTitle { title, count }),
        );
        self.review_items = items;
        self.review_cursor = 0;
    }

    /// Whether the review screen has anything to show at the chosen level
    fn review_has_items(&self) -> bool {
        // the items are rebuilt per level, so emptiness is the whole answer
        !self.review_items.is_empty()
    }

    /// Handles a key on the tier choice
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    async fn tier_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            // escape walks back to the picker
            KeyCode::Esc => self.screen = Screen::Picker,
            // pick the tier directly or toggle with the arrows; the review
            // items track the tier so level 0 never lists hidden signatures
            // (a no-op press re-redacts nothing, so key repeat stays cheap)
            KeyCode::Char('0') | KeyCode::Left => {
                if self.level != 0 {
                    self.level = 0;
                    self.rebuild_review_items();
                }
            }
            KeyCode::Char('1') | KeyCode::Right => {
                if self.level != 1 {
                    self.level = 1;
                    self.rebuild_review_items();
                }
            }
            // enter advances to the review when it has items, else the preview
            KeyCode::Enter => {
                if self.review_has_items() {
                    self.screen = Screen::Review;
                } else {
                    self.enter_preview();
                }
            }
            _ => {}
        }
    }

    /// Handles a key on the redaction review
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    fn review_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            // escape walks back to the tier choice
            KeyCode::Esc => self.screen = Screen::Tier,
            KeyCode::Up | KeyCode::Char('k') => self.move_review_cursor(true),
            KeyCode::Down | KeyCode::Char('j') => self.move_review_cursor(false),
            // space toggles the drop under the cursor
            KeyCode::Char(' ') => {
                let Some(item) = self.review_items.get(self.review_cursor) else {
                    return;
                };
                match item {
                    ReviewItem::Signature { tool, sig, .. } => {
                        let key = (tool.clone(), sig.clone());
                        if !self.dropped_sigs.remove(&key) {
                            self.dropped_sigs.insert(key);
                        }
                        self.invalidate_tier_sizes();
                    }
                    ReviewItem::TechniqueId { id } => {
                        if !self.dropped_ids.remove(id) {
                            self.dropped_ids.insert(id.clone());
                        }
                        self.invalidate_tier_sizes();
                    }
                    ReviewItem::EntryTitle { title, .. } => {
                        if !self.dropped_titles.remove(title) {
                            self.dropped_titles.insert(title.clone());
                        }
                        self.invalidate_tier_sizes();
                    }
                }
            }
            // `0` falls back to the content-free tier entirely
            KeyCode::Char('0') => {
                self.level = 0;
                self.rebuild_review_items();
                self.screen = Screen::Tier;
            }
            // `v` (or →) views the full multi-line signature under the cursor
            KeyCode::Char('v') | KeyCode::Right => {
                if let Some(ReviewItem::Signature {
                    tool,
                    sig,
                    count,
                    looped,
                }) = self.review_items.get(self.review_cursor)
                {
                    self.popup = Some(signature_popup(tool, sig, *count, *looped, None, None));
                }
            }
            // enter advances to a fresh preview over the final bytes
            KeyCode::Enter => self.enter_preview(),
            _ => {}
        }
    }

    /// Computes both tiers' exact byte sizes under the current drops
    ///
    /// Both finalizes share one timestamp, so the delta is exactly the
    /// content difference and never a few bytes of clock formatting.
    fn tier_sizes(&mut self) -> TierSizes {
        // reuse the cached sizes until a drop or selection change invalidates
        if let Some(sizes) = self.tier_sizes {
            return sizes;
        }
        // one whole-second stamp for both renders keeps the delta exact
        let now = export_stamp();
        let mut sizes = TierSizes::default();
        for (level, slot) in [(0u8, 0usize), (1u8, 1usize)] {
            // finalize this tier exactly like the preview and writer would
            let bundle = finalize(
                &self.rows,
                level,
                self.redactor.as_deref(),
                &self.dropped_sigs,
                &self.dropped_ids,
                &self.dropped_titles,
                now,
            );
            // the rendered byte length is the figure the screens advertise
            let bytes = bundle.to_bytes().map(|bytes| bytes.len()).unwrap_or(0);
            if slot == 0 {
                sizes.level0 = bytes;
            } else {
                // level 1 also counts the prose that survives the drops
                sizes.level1 = bytes;
                sizes.signatures = bundle
                    .sessions
                    .iter()
                    .map(|session| session.signatures.len())
                    .sum();
                sizes.titles = bundle
                    .sessions
                    .iter()
                    .flat_map(|session| session.forum.entries.iter())
                    .filter(|entry| entry.title.is_some())
                    .count();
            }
        }
        // cache for every later frame until the bytes can change again
        self.tier_sizes = Some(sizes);
        sizes
    }

    /// Drops the cached tier sizes after anything that changes the bytes
    fn invalidate_tier_sizes(&mut self) {
        self.tier_sizes = None;
    }

    /// Finalizes the bundle and enters the preview over its exact bytes
    fn enter_preview(&mut self) {
        // render the exact bytes the writer would write right now
        let bundle = finalize(
            &self.rows,
            self.level,
            self.redactor.as_deref(),
            &self.dropped_sigs,
            &self.dropped_ids,
            &self.dropped_titles,
            export_stamp(),
        );
        match bundle.to_bytes() {
            Ok(bytes) => {
                self.bytes = bytes;
                self.bundle = Some(bundle);
                self.scroll = 0;
                self.screen = Screen::Preview;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Handles a key on the preview
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    fn preview_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            // escape returns to the review (or the tier when it was skipped),
            // and the preview re-renders fresh bytes on the way back in
            KeyCode::Esc => {
                self.screen = if self.review_has_items() {
                    Screen::Review
                } else {
                    Screen::Tier
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll = self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(20),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(20),
            KeyCode::Home => self.scroll = 0,
            // `t` toggles the tier in place; 0/1 set it directly -- the
            // preview re-finalizes so the shown bytes stay the written bytes
            KeyCode::Char('t') | KeyCode::Char('0') | KeyCode::Char('1') => {
                self.level = match key.code {
                    KeyCode::Char('0') => 0,
                    KeyCode::Char('1') => 1,
                    _ => 1 - self.level.min(1),
                };
                self.rebuild_review_items();
                self.enter_preview();
            }
            // enter confirms: write the previewed bytes verbatim
            KeyCode::Enter => {
                match Bundle::write_bytes(&self.bytes, &self.out) {
                    Ok(()) => {
                        // keep the same summary line the headless path prints
                        let line = self
                            .bundle
                            .as_ref()
                            .map(|bundle| summary_line(bundle, &self.bytes, &self.out))
                            .unwrap_or_default();
                        self.done_line = Some(line);
                        self.screen = Screen::Done;
                    }
                    Err(err) => self.error = Some(err.to_string()),
                }
            }
            _ => {}
        }
    }

    /// Renders the whole wizard frame
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // one header row, the body, and one footer row
        let body = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        self.render_header(frame, Rect { height: 1, ..area });
        match self.screen {
            Screen::Picker => self.render_picker(frame, body),
            Screen::Tier => self.render_tier(frame, body),
            Screen::Review => self.render_review(frame, body),
            Screen::Preview => self.render_preview(frame, body),
            Screen::Done => self.render_done(frame, body),
        }
        // the pop-out floats over whatever the screen drew
        if let Some(popup) = &mut self.popup {
            popup.render(frame, body);
        }
        let footer = Rect {
            y: area.y + area.height.saturating_sub(1),
            height: 1,
            ..area
        };
        self.render_footer(frame, footer);
    }

    /// Renders the step-crumb header
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The header row
    fn render_header(&self, frame: &mut Frame, area: Rect) {
        // light the current step inside the crumb trail the WALK table drives
        let mut spans = vec![Span::styled(" telemetry export ", self.theme.title)];
        for (screen, label) in WALK {
            spans.push(Span::raw("→ "));
            // the shell theme keeps both telemetry TUIs in one visual family
            let style = if screen == self.screen {
                self.theme.accent.add_modifier(Modifier::BOLD)
            } else {
                self.theme.dim
            };
            spans.push(Span::styled(format!("{label} "), style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Renders the per-screen key hints and any pending error
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The footer row
    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        // a pending error outranks the hints until the next action
        let text = if let Some(error) = &self.error {
            format!(" ⚠ {error}")
        } else if let Some((done, total)) = self.progress {
            format!(" scanning sessions… {done}/{total}")
        } else {
            match self.screen {
                Screen::Picker => {
                    " ↑/↓ move · space toggle · a all/none · r rescan · enter continue · q quit"
                        .to_owned()
                }
                Screen::Tier => {
                    " ←/→ or 0/1 pick level · enter continue · esc back · q quit".to_owned()
                }
                Screen::Review => {
                    " ↑/↓ move · space drop/keep · 0 fall back to level 0 · enter preview · esc back"
                        .to_owned()
                }
                Screen::Preview => {
                    " ↑/↓/PgUp/PgDn scroll · enter WRITE the previewed bytes · esc back · q quit"
                        .to_owned()
                }
                Screen::Done => " any key quits".to_owned(),
            }
        };
        let style = if self.error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(Paragraph::new(text).style(style), area);
    }

    /// Renders the multi-select session picker
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_picker(&mut self, frame: &mut Frame, area: Rect) {
        // clamp the cursor as sessions appear and vanish live
        self.cursor = self.cursor.min(self.sessions.len().saturating_sub(1));
        let order = self.picker_order();
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "  sel  {:<28} {:>8} {:>14} {:>10}  {}",
                "session", "age", "tokens", "active", "live"
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        // window the rows around the cursor
        let height = area.height.saturating_sub(1) as usize;
        let first = self.cursor.saturating_sub(height.saturating_sub(1) / 2);
        for (index, summary) in order.iter().enumerate().skip(first).take(height) {
            let mark = if self.selected.contains(&summary.name) {
                "[x]"
            } else {
                "[ ]"
            };
            let text = format!(
                "  {mark}  {:<28} {:>8} {:>14} {:>10}  {}",
                clip(&summary.name, 28),
                ago(summary.last_ts),
                with_thousands(summary.tokens()),
                fmt_duration(summary.active_ms),
                if summary.live { "live" } else { "" }
            );
            let style = if index == self.cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Renders the tier choice with its plain-language diff
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_tier(&mut self, frame: &mut Frame, area: Rect) {
        // both tiers' exact sizes, cached until the drops or rows change
        let sizes = self.tier_sizes();
        // mark the chosen tier and spell out exactly what each one ships
        let mark = |level: u8| if self.level == level { "(•)" } else { "( )" };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!(" {} level 0 — strictly content-free (default)", mark(0)),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("       counts, token totals, tool-name histograms, failure rates,"),
            Line::from("       durations, decision-point and detector-outcome tallies."),
            Line::from("       the only strings: tool names, event kinds, enum values,"),
            Line::from("       role names, model/version ids, technique ids, timestamps,"),
            Line::from("       and hashes."),
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    " {} level 1 — adds failure signatures and forum titles",
                    mark(1)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("       bounded multi-line, defanged, number-scrubbed prefixes of"),
            Line::from("       FAILING tool results (up to 16 lines / 1,500 chars), plus"),
            Line::from("       the forum end-state's question titles and task descriptions"),
            Line::from("       — each redacted and individually droppable on review."),
            Line::from("       still excluded: everything level 0 excludes — chat and"),
            Line::from("       agent messages, tool arguments and successful results,"),
            Line::from("       forum bodies, reports, scripts, file names, and paths."),
            Line::from(""),
            Line::from(format!(
                "   level 0 — {} bytes      level 1 — {} bytes (+{}, {} signature(s), {} title(s))",
                with_thousands(sizes.level0 as u64),
                with_thousands(sizes.level1 as u64),
                with_thousands(sizes.level1.saturating_sub(sizes.level0) as u64),
                sizes.signatures,
                sizes.titles,
            )),
            Line::from(""),
            Line::from(format!(
                "   selected sessions: {}   output: {}",
                self.rows.len(),
                self.out.display()
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Renders the redaction review list
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_review(&mut self, frame: &mut Frame, area: Rect) {
        // seeing the review at level 1 clears the preview's unreviewed warning
        if self.level == 1 {
            self.reviewed_at_level1 = true;
        }
        // clamp the cursor against rebuilt lists
        self.review_cursor = self
            .review_cursor
            .min(self.review_items.len().saturating_sub(1));
        let mut lines = vec![Line::from(Span::styled(
            " keep  what ships (space drops an item; dropped ids fold under `other`)",
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        let height = area.height.saturating_sub(1) as usize;
        let first = self
            .review_cursor
            .saturating_sub(height.saturating_sub(1) / 2);
        for (index, item) in self
            .review_items
            .iter()
            .enumerate()
            .skip(first)
            .take(height)
        {
            let (dropped, text) = match item {
                ReviewItem::Signature {
                    tool,
                    sig,
                    count,
                    looped,
                } => {
                    let dropped = self.dropped_sigs.contains(&(tool.clone(), sig.clone()));
                    let flag = if *looped { " loop!" } else { "" };
                    // list rows show the first line; the detail pop-out (a
                    // later stage) carries the full multi-line signature
                    let (first, more) = crate::handlers::ai::telemetry::sig::sig_summary(sig);
                    let badge = if more > 0 {
                        format!(" (+{more} lines)")
                    } else {
                        String::new()
                    };
                    (
                        dropped,
                        format!("{:<18} {count:>5}×{flag}  {first}{badge}", clip(tool, 18)),
                    )
                }
                ReviewItem::TechniqueId { id } => {
                    let dropped = self.dropped_ids.contains(id);
                    (dropped, format!("technique id (non-native): {id}"))
                }
                ReviewItem::EntryTitle { title, count } => {
                    let dropped = self.dropped_titles.contains(title);
                    (dropped, format!("forum title {count:>3}×  {title}"))
                }
            };
            let mark = if dropped { "[ ]" } else { "[x]" };
            let mut style = if index == self.review_cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            if dropped {
                style = style.add_modifier(Modifier::CROSSED_OUT);
            }
            lines.push(Line::from(Span::styled(
                format!(
                    " {mark}   {}",
                    clip(&text, area.width.saturating_sub(8) as usize)
                ),
                style,
            )));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Renders the exact-bytes preview
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_preview(&mut self, frame: &mut Frame, area: Rect) {
        // size both tiers before borrowing the preview bytes below
        let sizes = self.tier_sizes();
        // the preview is the bundle: render the exact bytes, windowed
        let text = String::from_utf8_lossy(&self.bytes);
        let total = text.lines().count();
        // the sha header and delta line always render, the unreviewed
        // warning conditionally adds a third row -- the body window shrinks
        // to fit so max scroll still reaches the bundle's last line
        let warn = self.level == 1
            && (sizes.signatures > 0 || sizes.titles > 0)
            && !self.reviewed_at_level1;
        let header_rows = 2 + usize::from(warn);
        let height = (area.height as usize).saturating_sub(header_rows);
        self.scroll = self.scroll.min(total.saturating_sub(height.min(total)));
        let mut lines = vec![Line::from(Span::styled(
            format!(
                " {} bytes → {} (sha256 {:.16}…)  line {}/{total}",
                self.bytes.len(),
                self.out.display(),
                format!("{:x}", Sha256::digest(&self.bytes)),
                self.scroll + 1,
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        // the tier delta, phrased for whichever level is showing
        let delta = with_thousands(sizes.level1.saturating_sub(sizes.level0) as u64);
        lines.push(Line::from(Span::styled(
            if self.level == 1 {
                format!(
                    " level 1 adds {} signature(s), {} title(s), +{delta} bytes vs level 0 — t toggles",
                    sizes.signatures, sizes.titles
                )
            } else {
                format!(
                    " level 1 would add {} signature(s), {} title(s), +{delta} bytes — t toggles",
                    sizes.signatures, sizes.titles
                )
            },
            self.theme.dim,
        )));
        // warn when level-1 prose was never reviewed on this selection
        if warn {
            lines.push(Line::from(Span::styled(
                format!(
                    " ⚠ {} signature(s) / {} title(s) not yet reviewed — esc to review/drop",
                    sizes.signatures, sizes.titles
                ),
                self.theme.warn,
            )));
        }
        for line in text.lines().skip(self.scroll).take(height) {
            lines.push(Line::from(clip(line, area.width as usize)));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Renders the written-bundle summary
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_done(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  bundle written → {}", self.out.display()),
                self.theme.ok.add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("  {}", self.done_line.clone().unwrap_or_default())),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// Clips (or pads) a string to an exact display width in characters
///
/// # Arguments
///
/// * `text` - The text to fit
/// * `width` - The exact character width to fit it into
fn clip(text: &str, width: usize) -> String {
    // count characters, not bytes, so unicode names stay aligned
    let count = text.chars().count();
    if count > width {
        // leave room for the ellipsis marker when clipping
        let kept: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{kept}…")
    } else {
        // pad short text out to the exact width
        format!("{text}{}", " ".repeat(width - count))
    }
}

/// Formats how long ago an epoch stamp was, coarsely
///
/// # Arguments
///
/// * `ts` - Milliseconds since the Unix epoch, when known
fn ago(ts: Option<u64>) -> String {
    // no stamp means nothing sensible to say
    let Some(ts) = ts else {
        return "—".to_owned();
    };
    // a future stamp (clock skew) reads as right now
    let now = Utc::now().timestamp_millis().max(0) as u64;
    let minutes = now.saturating_sub(ts) / 60_000;
    // climb the units coarsely; precision past days helps nobody here
    if minutes < 1 {
        "now".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / (60 * 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stderr tail format is a contract; this test pins it
    #[test]
    fn summary_line_format_is_pinned() {
        // a tiny level-1 bundle with one signature
        let mut session = SessionTelemetry::default();
        session.signatures.push(SignatureCluster {
            tool: "run_script".to_owned(),
            sig: "boom N".to_owned(),
            count: 2,
            looped: false,
        });
        let bundle = Bundle {
            bundle_version: BUNDLE_VERSION,
            level: 1,
            created_at: None,
            tool_version: "test".to_owned(),
            signature_recipe: SIGNATURE_RECIPE,
            sessions: vec![session],
        };
        let bytes = b"exact bytes";
        let line = summary_line(&bundle, bytes, Path::new("/tmp/b.json"));
        // the pinned shape: fixed keys, fixed order, hex digest of the bytes
        let digest = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(
            line,
            format!(
                "telemetry: sessions=1 level=1 signatures=1 bytes=11 sha256={digest} out=/tmp/b.json"
            )
        );
    }

    /// One selection renders byte-identical bundles whatever the row order
    #[test]
    fn finalize_orders_sessions_for_byte_parity() {
        // the same two rows in both orders, finalized at one timestamp
        let first = SessionTelemetry {
            session_id: "b".repeat(64),
            ..SessionTelemetry::default()
        };
        let second = SessionTelemetry {
            session_id: "a".repeat(64),
            ..SessionTelemetry::default()
        };
        let now = chrono::Utc::now();
        let sigs = BTreeSet::new();
        let ids = BTreeSet::new();
        let titles = BTreeSet::new();
        let forward = finalize(
            &[first.clone(), second.clone()],
            0,
            None,
            &sigs,
            &ids,
            &titles,
            now,
        );
        let reversed = finalize(&[second, first], 0, None, &sigs, &ids, &titles, now);
        // the wizard's picker order and the headless scan order both render
        // the same bytes, so the stderr sha certifies the selection itself
        assert_eq!(
            forward.to_bytes().expect("forward bytes"),
            reversed.to_bytes().expect("reversed bytes"),
        );
    }

    /// Level 0 strips signatures; level 1 keeps, drops, redacts, and merges
    #[test]
    fn finalize_applies_tier_drops_and_redaction() {
        // one row with two signatures and one non-native technique id
        let mut row = SessionTelemetry {
            signatures: vec![
                SignatureCluster {
                    tool: "run_script".to_owned(),
                    sig: "secretcorp exploded".to_owned(),
                    count: 3,
                    looped: true,
                },
                SignatureCluster {
                    tool: "run_script".to_owned(),
                    sig: "dropped entirely".to_owned(),
                    count: 1,
                    looped: false,
                },
            ],
            ..SessionTelemetry::default()
        };
        row.detectors.insert(
            "T1027".to_owned(),
            DetectorTally {
                confirmed: 1,
                ..DetectorTally::default()
            },
        );
        row.detectors.insert(
            "SecretCorp.Rule".to_owned(),
            DetectorTally {
                disputed: 2,
                ..DetectorTally::default()
            },
        );
        row.detectors.insert(
            "Dropped.Rule".to_owned(),
            DetectorTally {
                ignored: 4,
                ..DetectorTally::default()
            },
        );
        // a deny list covering the fake family name
        let redactor = LibraryRedactor::new(false, vec!["secretcorp".to_owned()], Vec::new())
            .expect("a redactor from one token");
        let dropped_sigs =
            BTreeSet::from([("run_script".to_owned(), "dropped entirely".to_owned())]);
        let dropped_ids = BTreeSet::from(["Dropped.Rule".to_owned()]);
        // level 0 ships no signatures at all but still guards technique ids
        let level0 = finalize(
            std::slice::from_ref(&row),
            0,
            Some(&redactor),
            &dropped_sigs,
            &dropped_ids,
            &BTreeSet::new(),
            Utc::now(),
        );
        assert!(level0.sessions[0].signatures.is_empty());
        assert!(!level0.sessions[0].detectors.contains_key("SecretCorp.Rule"));
        assert_eq!(level0.sessions[0].detectors["T1027"].confirmed, 1);
        assert_eq!(level0.sessions[0].detectors[OTHER_TECHNIQUES].ignored, 4);
        // level 1 keeps the surviving signature, redacted
        let level1 = finalize(
            std::slice::from_ref(&row),
            1,
            Some(&redactor),
            &dropped_sigs,
            &dropped_ids,
            &BTreeSet::new(),
            Utc::now(),
        );
        let sigs = &level1.sessions[0].signatures;
        assert_eq!(sigs.len(), 1);
        // the token is gone AND its reversible fingerprint flattened away
        assert_eq!(sigs[0].sig, "[denied] exploded");
        assert!(!sigs[0].sig.contains("fam-"), "got {:?}", sigs[0].sig);
        assert_eq!(sigs[0].count, 3);
        assert!(sigs[0].looped);
        // the version and level stamps ride the bundle itself
        assert_eq!(level1.bundle_version, BUNDLE_VERSION);
        assert_eq!(level1.level, 1);
    }

    /// Builds a wizard app over a scratch root for key-routing tests
    fn test_app() -> ExportApp {
        let export = TelemetryExport {
            level: 0,
            session: Vec::new(),
            sessions_root: None,
            redact_tokens: None,
            out: Some(PathBuf::from("/tmp/unused.json")),
            yes: false,
        };
        ExportApp::new(PathBuf::from("/tmp"), PricingCtx::default(), &export, None)
    }

    /// Builds a pressed key event
    ///
    /// # Arguments
    ///
    /// * `code` - The key code to press
    fn press(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
        crossterm::event::KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Escape closes an open pop-out without also navigating the wizard
    #[tokio::test]
    async fn escape_closes_popup_before_navigating() {
        let mut app = test_app();
        app.screen = Screen::Tier;
        // help opens on any screen
        app.handle_key(press(KeyCode::Char('?'))).await;
        assert!(app.popup.is_some());
        // the first escape closes the pop-out and stays on the tier screen
        app.handle_key(press(KeyCode::Esc)).await;
        assert!(app.popup.is_none());
        assert_eq!(app.screen, Screen::Tier);
        // only the next escape walks back to the picker
        app.handle_key(press(KeyCode::Esc)).await;
        assert_eq!(app.screen, Screen::Picker);
    }

    /// The wizard's help renders the screen's page, even at a narrow size
    #[tokio::test]
    async fn help_renders_the_tier_page() {
        let mut app = test_app();
        app.screen = Screen::Tier;
        app.handle_key(press(KeyCode::Char('?'))).await;
        assert!(app.popup.is_some());
        // the tier page spells out the posture at the drive matrix's floor
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("frame");
        let text = format!("{:?}", terminal.backend().buffer());
        assert!(text.contains("help — tier"), "got {text:?}");
        assert!(text.contains("content-free"), "the body must render");
    }

    /// Space keeps toggling picker selections; the palette is explorer-only
    #[tokio::test]
    async fn space_still_toggles_in_the_picker() {
        let mut app = test_app();
        // one scanned session, preselected like a live scan would
        app.sessions.insert(
            "alpha".to_owned(),
            SessionSummary {
                name: "alpha".to_owned(),
                ..SessionSummary::default()
            },
        );
        app.selected.insert("alpha".to_owned());
        // space toggles the cursor's session off, then back on
        app.handle_key(press(KeyCode::Char(' '))).await;
        assert!(app.selected.is_empty());
        app.handle_key(press(KeyCode::Char(' '))).await;
        assert!(app.selected.contains("alpha"));
        assert!(app.popup.is_none(), "space must never open a palette here");
    }

    /// The tier toggle re-finalizes in place, keeps drops, and deltas exactly
    #[tokio::test]
    async fn tier_toggle_refinalizes_and_keeps_drops() {
        use crate::handlers::ai::telemetry::bundle::SignatureCluster;
        let mut app = test_app();
        // one row with two signatures, one of them reviewed out at level 1
        let row = SessionTelemetry {
            signatures: vec![
                SignatureCluster {
                    tool: "run_script".to_owned(),
                    sig: "kept".to_owned(),
                    count: 2,
                    looped: false,
                },
                SignatureCluster {
                    tool: "run_script".to_owned(),
                    sig: "dropped".to_owned(),
                    count: 1,
                    looped: false,
                },
            ],
            ..SessionTelemetry::default()
        };
        app.rows = vec![row];
        app.level = 1;
        app.rebuild_review_items();
        app.dropped_sigs
            .insert(("run_script".to_owned(), "dropped".to_owned()));
        app.invalidate_tier_sizes();
        // enter the preview at level 1, then toggle down and back up
        app.enter_preview();
        let level1_bytes = app.bytes.clone();
        app.handle_key(press(KeyCode::Char('t'))).await;
        assert_eq!(app.level, 0);
        let level0_bytes = app.bytes.clone();
        app.handle_key(press(KeyCode::Char('t'))).await;
        assert_eq!(app.level, 1);
        // the drop survived the round trip: the dropped cluster never ships
        let text = String::from_utf8(app.bytes.clone()).expect("utf8");
        assert!(text.contains("kept"));
        assert!(!text.contains("dropped\""), "the dropped sig must stay out");
        // the shown delta is exactly the two renders' difference (the cached
        // sizes share one timestamp, so clock formatting can never skew it)
        let sizes = app.tier_sizes();
        assert_eq!(
            sizes.level1 - sizes.level0,
            level1_bytes.len() - level0_bytes.len(),
            "the delta must be exact"
        );
        assert_eq!(sizes.signatures, 1, "one signature survives the drop");
        // writing after a toggle writes exactly the shown bytes
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("t.json");
        app.out = out.clone();
        app.handle_key(press(KeyCode::Enter)).await;
        assert_eq!(std::fs::read(&out).expect("written"), app.bytes);
    }

    /// Placeholders flatten even when spliced inside identifiers
    #[test]
    fn placeholders_flatten_inside_identifiers() {
        // the redactor splices placeholders mid-word, where a word-boundary
        // regex would leave the reversible fingerprint in place
        let redactor = LibraryRedactor::new(false, vec!["secretcorp".to_owned()], Vec::new())
            .expect("a redactor from one token");
        assert_eq!(
            export_redact(Some(&redactor), "MAL_SecretCorp_Loader"),
            "MAL_[denied]_Loader"
        );
        assert_eq!(
            export_redact(Some(&redactor), "secretcorpLoader failed"),
            "[denied]Loader failed"
        );
        // the standalone shape still flattens too
        assert_eq!(
            export_redact(Some(&redactor), "secretcorp ran"),
            "[denied] ran"
        );
    }

    /// Max scroll still reaches the bundle's last line under the header rows
    #[tokio::test]
    async fn preview_max_scroll_reaches_the_last_line() {
        use crate::handlers::ai::telemetry::bundle::SignatureCluster;
        let mut app = test_app();
        // level 1 with an unreviewed signature shows all three header rows
        let row = SessionTelemetry {
            signatures: vec![SignatureCluster {
                tool: "run_script".to_owned(),
                sig: "boom".to_owned(),
                count: 1,
                looped: false,
            }],
            ..SessionTelemetry::default()
        };
        app.rows = vec![row];
        app.level = 1;
        app.screen = Screen::Preview;
        // forty lines of body ending in a marker, far taller than the window
        let body: Vec<String> = (0..40).map(|index| format!("line{index}")).collect();
        app.bytes = format!("{}\nzzz_last_line", body.join("\n")).into_bytes();
        app.scroll = usize::MAX;
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("frame");
        let text = format!("{:?}", terminal.backend().buffer());
        assert!(text.contains("not yet reviewed"), "the warning row renders");
        assert!(text.contains("zzz_last_line"), "got {text:?}");
    }

    /// The preview warns when level-1 prose was never reviewed
    #[tokio::test]
    async fn preview_warns_on_unreviewed_level_one() {
        use crate::handlers::ai::telemetry::bundle::SignatureCluster;
        let mut app = test_app();
        let row = SessionTelemetry {
            signatures: vec![SignatureCluster {
                tool: "run_script".to_owned(),
                sig: "boom".to_owned(),
                count: 1,
                looped: false,
            }],
            ..SessionTelemetry::default()
        };
        app.rows = vec![row];
        // jump straight from the tier screen to the preview at level 1
        app.level = 1;
        app.rebuild_review_items();
        app.enter_preview();
        assert!(!app.reviewed_at_level1);
        let sizes = app.tier_sizes();
        assert!(sizes.signatures > 0);
        // rendering the review at level 1 clears the warning condition
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        app.screen = Screen::Review;
        terminal.draw(|frame| app.render(frame)).expect("frame");
        assert!(app.reviewed_at_level1);
        // a fresh selection means unseen prose: the warning re-arms
        app.assemble_selection().await;
        assert!(!app.reviewed_at_level1);
    }

    /// `v` views signatures on the review screen and no-ops on other items
    #[tokio::test]
    async fn review_v_views_signatures_only() {
        let mut app = test_app();
        app.level = 1;
        app.screen = Screen::Review;
        app.review_items = vec![
            ReviewItem::TechniqueId {
                id: "Capa.Rule_1".to_owned(),
            },
            ReviewItem::Signature {
                tool: "run_script".to_owned(),
                sig: "boom\nline two".to_owned(),
                count: 3,
                looped: false,
            },
        ];
        // on the technique id, v opens nothing
        app.handle_key(press(KeyCode::Char('v'))).await;
        assert!(app.popup.is_none(), "v must no-op on technique ids");
        // on the signature, v opens the detail pop-out
        app.review_cursor = 1;
        app.handle_key(press(KeyCode::Char('v'))).await;
        assert!(app.popup.is_some());
    }

    /// A duplicated --session name selects once instead of failing falsely
    #[test]
    fn duplicate_session_names_select_once() {
        let summary = SessionSummary {
            name: "dup".to_owned(),
            ..SessionSummary::default()
        };
        let selected = select_summaries(vec![summary], &["dup".to_owned(), "dup".to_owned()])
            .expect("duplicates are one selection");
        assert_eq!(selected.len(), 1);
        // a genuinely missing name still fails by name
        let err = select_summaries(Vec::new(), &["ghost".to_owned()])
            .expect_err("missing names must fail");
        assert!(err.to_string().contains("ghost"));
    }

    /// Review items track the tier and list the redacted strings
    #[test]
    fn review_items_track_tier_and_redaction() {
        use crate::handlers::ai::telemetry::bundle::DetectorTally;
        // an app over a scratch root with a deny token loaded
        let export = TelemetryExport {
            level: 0,
            session: Vec::new(),
            sessions_root: None,
            redact_tokens: None,
            out: Some(PathBuf::from("/tmp/unused.json")),
            yes: false,
        };
        let redactor = LibraryRedactor::new(false, vec!["secretcorp".to_owned()], Vec::new())
            .expect("a redactor from one token");
        let mut app = ExportApp::new(
            PathBuf::from("/tmp"),
            PricingCtx::default(),
            &export,
            Some(redactor),
        );
        // one row with a tokened signature and a non-native technique id
        let mut row = SessionTelemetry::default();
        row.signatures.push(SignatureCluster {
            tool: "run_script".to_owned(),
            sig: "secretcorp exploded".to_owned(),
            count: 2,
            looped: false,
        });
        row.detectors
            .insert("Capa.Rule_1".to_owned(), DetectorTally::default());
        app.rows = vec![row];
        // level 0 reviews only the technique id; no hidden signature rows
        app.rebuild_review_items();
        assert_eq!(app.review_items.len(), 1);
        assert!(matches!(
            &app.review_items[0],
            ReviewItem::TechniqueId { id } if id == "Capa.Rule_1"
        ));
        // level 1 adds the signature, already redacted and flattened --
        // exactly the string that would ship, so drops key what was reviewed
        app.level = 1;
        app.rebuild_review_items();
        assert_eq!(app.review_items.len(), 2);
        assert!(app.review_items.iter().any(|item| matches!(
            item,
            ReviewItem::Signature { sig, .. } if sig == "[denied] exploded"
        )));
    }

    /// A headless-style end-to-end: synthetic store → bundle file → parse back
    #[tokio::test]
    async fn export_pipeline_round_trips_a_synthetic_store() {
        use thorpal::{TranscriptItem, TranscriptLine};
        // one synthetic session: a transcript with a failing call and a
        // detector-seeded forum
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("case-alpha");
        std::fs::create_dir_all(&dir).expect("session dir");
        let lines = [
            TranscriptLine {
                ts: Some(1_000),
                item: TranscriptItem::ToolCall {
                    agent: "main".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "run_script".to_owned(),
                    args: "{}".to_owned(),
                },
            },
            TranscriptLine {
                ts: Some(3_000),
                item: TranscriptItem::ToolResult {
                    agent: "main".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "run_script".to_owned(),
                    result: "script error: line 42".to_owned(),
                    success: false,
                },
            },
            TranscriptLine {
                ts: Some(4_000),
                item: TranscriptItem::ToolsOffered {
                    role: "main".to_owned(),
                    tools: vec!["run_script".to_owned(), "think".to_owned()],
                    version: "9.9.9".to_owned(),
                },
            },
        ];
        let encoded: String = lines
            .iter()
            .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
            .collect();
        std::fs::write(dir.join("transcript.jsonl"), encoded).expect("transcript");
        // run the headless pipeline against the synthetic root
        let export = TelemetryExport {
            level: 1,
            session: Vec::new(),
            sessions_root: Some(root.path().to_path_buf()),
            redact_tokens: None,
            out: Some(root.path().join("bundle.json.gz")),
            yes: true,
        };
        let pricing = PricingCtx::default();
        headless(root.path(), &pricing, &export, None)
            .await
            .expect("the headless export should succeed");
        // the written bundle parses back with the folded telemetry intact
        let bundle = Bundle::read(&root.path().join("bundle.json.gz")).expect("read back");
        assert_eq!(bundle.bundle_version, BUNDLE_VERSION);
        assert_eq!(bundle.level, 1);
        assert_eq!(bundle.sessions.len(), 1);
        let session = &bundle.sessions[0];
        // the name never ships; its hash does
        assert_eq!(
            session.session_id,
            format!("{:x}", Sha256::digest(b"case-alpha"))
        );
        assert_eq!(session.tools["run_script"].calls, 1);
        assert_eq!(session.tools["run_script"].fails, 1);
        assert_eq!(session.tools["run_script"].p50_ms, Some(2_000));
        assert_eq!(session.harness_version.as_deref(), Some("9.9.9"));
        assert_eq!(session.tools_offered["main"], ["run_script", "think"]);
        assert_eq!(session.signatures.len(), 1);
        assert_eq!(session.signatures[0].sig, "script error: line N");
        // no telemetry summary text leaked: the whole document carries no
        // session name and no raw result text
        let bytes = bundle.to_bytes().expect("bytes");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(!text.contains("case-alpha"));
        assert!(!text.contains("line 42"));
    }

    /// Walks every JSON string (keys and values) with an assertion
    ///
    /// # Arguments
    ///
    /// * `value` - The JSON value to walk
    /// * `check` - The assertion run on every string
    fn walk_strings(value: &serde_json::Value, check: &dyn Fn(&str)) {
        match value {
            serde_json::Value::String(text) => check(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    walk_strings(item, check);
                }
            }
            serde_json::Value::Object(map) => {
                for (key, item) in map {
                    check(key);
                    walk_strings(item, check);
                }
            }
            _ => {}
        }
    }

    /// A level-0 bundle built from hostile inputs ships none of their text
    ///
    /// Every text-bearing input is seeded with a canary marker; the exported
    /// level-0 document must carry no marker anywhere, and every string in it
    /// (key or value) must stay short, single-line vocabulary -- the
    /// structural shape of enum values, names, ids, hashes, and timestamps.
    #[tokio::test]
    async fn level_zero_bundles_are_content_free() {
        use thorpal::{
            Comment, Kind, ParentRef, PauseKind, Post, Stance, TranscriptItem, TranscriptLine,
        };
        // a session whose every text surface carries the canary
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("CANARY-session-name");
        std::fs::create_dir_all(&dir).expect("session dir");
        let items = vec![
            TranscriptItem::User {
                text: "CANARY user prompt".to_owned(),
                scope: Some("CANARY-side".to_owned()),
            },
            TranscriptItem::SubAgentStarted {
                name: "ReverseEngineer(Io)".to_owned(),
                role: Some("ReverseEngineer".to_owned()),
                goal: "CANARY goal".to_owned(),
                swarm: None,
                resumed: false,
            },
            TranscriptItem::SubAgentMessage {
                agent: "ReverseEngineer(Io)".to_owned(),
                text: "CANARY finding".to_owned(),
            },
            TranscriptItem::ToolCall {
                agent: "ReverseEngineer(Io)".to_owned(),
                call_id: "c1".to_owned(),
                tool: "run_script".to_owned(),
                args: "{\"script\":\"CANARY args\"}".to_owned(),
            },
            TranscriptItem::ToolResult {
                agent: "ReverseEngineer(Io)".to_owned(),
                call_id: "c1".to_owned(),
                tool: "run_script".to_owned(),
                result: "CANARY failing result".to_owned(),
                success: false,
            },
            TranscriptItem::ToolResult {
                agent: "ReverseEngineer(Io)".to_owned(),
                call_id: "c2".to_owned(),
                tool: "run_script".to_owned(),
                result: "CANARY successful result".to_owned(),
                success: true,
            },
            TranscriptItem::TurnFailed {
                agent: "main".to_owned(),
                error: "CANARY error body".to_owned(),
                request_id: Some("req-1".to_owned()),
            },
            TranscriptItem::LlmRetry {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempt: 1,
                max_attempts: 12,
                error: "CANARY 429".to_owned(),
                request_id: None,
                backoff_ms: 100,
            },
            TranscriptItem::ProviderPaused {
                pause: PauseKind::Auth,
                agent: "main".to_owned(),
                error: "CANARY 401".to_owned(),
                request_id: None,
            },
            TranscriptItem::AnalystAsk {
                agent: "main".to_owned(),
                question: "CANARY question".to_owned(),
                chosen: "CANARY answer".to_owned(),
                source: "auto_default".to_owned(),
            },
            TranscriptItem::ReviewFlag {
                agent: "main".to_owned(),
                title: "CANARY flag title".to_owned(),
            },
            TranscriptItem::SubAgentTerminated {
                name: "ReverseEngineer(Io)".to_owned(),
                reason: "CANARY reason".to_owned(),
                class: "max_turns".to_owned(),
                salvage_post: Some("CANARY-salvage".to_owned()),
            },
            TranscriptItem::ReportDegraded {
                rung: 1,
                detail: "CANARY detail".to_owned(),
            },
            TranscriptItem::DetectorsRun {
                sample: Some("CANARY-sample.bin".to_owned()),
                detectors: vec![thorpal::DetectorRunLine {
                    name: "packing".to_owned(),
                    technique: "T1027.002".to_owned(),
                    hits: 0,
                    failed: false,
                }],
                version: "9.9.9".to_owned(),
            },
        ];
        let encoded: String = items
            .into_iter()
            .map(|item| {
                let line = TranscriptLine {
                    ts: Some(1_000),
                    item,
                };
                format!("{}\n", serde_json::to_string(&line).unwrap())
            })
            .collect();
        std::fs::write(dir.join("transcript.jsonl"), encoded).expect("transcript");
        // a forum whose prose is hostile too (ids parse from the shaped lines)
        let forum = thorpal::ForumSnapshot {
            posts: vec![
                Post {
                    name: "fn-00401000".to_owned(),
                    author: thorpal::DETECTOR_AUTHOR.to_owned(),
                    title: "CANARY lead title".to_owned(),
                    body: "CANARY lead prose.\nTechniques: T1027.".to_owned(),
                    kind: Kind::Statement,
                    updated: 0,
                },
                Post {
                    name: "agent-note".to_owned(),
                    author: "ReverseEngineer(Io)".to_owned(),
                    title: "CANARY post title".to_owned(),
                    body: "CANARY post body".to_owned(),
                    kind: Kind::Question(thorpal::QuestionStatus::Unsolved),
                    updated: 0,
                },
                // a container question embedding archive entry names shaped
                // like the id-bearing template lines
                Post {
                    name: "q-container-payload".to_owned(),
                    author: thorpal::DETECTOR_AUTHOR.to_owned(),
                    title: "CANARY container question".to_owned(),
                    body: "Entries:\n- CANARY-ops @ q3.doc (123 bytes)\nTechniques: T1027."
                        .to_owned(),
                    kind: Kind::Question(thorpal::QuestionStatus::Unsolved),
                    updated: 0,
                },
                // a sibling question whose title carries the file-name suffix
                Post {
                    name: "s2-q-unpack".to_owned(),
                    author: thorpal::DETECTOR_AUTHOR.to_owned(),
                    title: "Can it unpack? — CANARY-file.dll".to_owned(),
                    body: "CANARY scoped body".to_owned(),
                    kind: Kind::Question(thorpal::QuestionStatus::Unsolved),
                    updated: 0,
                },
            ],
            comments: vec![
                Comment {
                    id: 1,
                    parent: ParentRef::Post("fn-00401000".to_owned()),
                    author: "ReverseEngineer(Io)".to_owned(),
                    body: "CANARY comment body".to_owned(),
                    stance: Stance::Supports,
                    kind: Kind::Statement,
                    updated: 0,
                },
                // a comment-question: its body stands in as local-only prose
                Comment {
                    id: 2,
                    parent: ParentRef::Post("agent-note".to_owned()),
                    author: "ReverseEngineer(Io)".to_owned(),
                    body: "CANARY comment question\nCANARY second line".to_owned(),
                    stance: Stance::Neutral,
                    kind: Kind::Question(thorpal::QuestionStatus::Unsolved),
                    updated: 0,
                },
                // a detector finding whose later lines carry hostile text
                Comment {
                    id: 3,
                    parent: ParentRef::Post("fn-00401000".to_owned()),
                    author: thorpal::DETECTOR_AUTHOR.to_owned(),
                    body: "[T1027 CANARY name] CANARY finding\nSite: 0x1 | confidence: unsure | suspicion: 40"
                        .to_owned(),
                    stance: Stance::Neutral,
                    kind: Kind::Statement,
                    updated: 0,
                },
            ],
            tasks: vec![thorpal::Task {
                id: 1,
                parent: ParentRef::Post("agent-note".to_owned()),
                author: "ReverseEngineer(Io)".to_owned(),
                description: "CANARY task description".to_owned(),
                completed: None,
                updated: 0,
            }],
            asks: Vec::new(),
            flags: Vec::new(),
            // a forged provenance block claiming prose-bearing class fields
            detector_provenance: vec![thorpal::NodeProvenance {
                node: ParentRef::Comment(3),
                shape: "finding".to_owned(),
                question_origin: None,
                findings: vec![thorpal::FindingProvenance {
                    technique: "T1027".to_owned(),
                    confidence: "unsure".to_owned(),
                    suspicion: 40,
                    source: "CANARY-source".to_owned(),
                    detection: Some("CANARY-detection".to_owned()),
                    sites: 0,
                    artifact_class: Some("CANARY-class".to_owned()),
                    suppressed: false,
                }],
            }],
            next_comment_id: 4,
            next_task_id: 2,
            next_ask_id: 0,
            next_flag_id: 0,
            finish_requested: false,
        };
        std::fs::write(
            dir.join("forum.json"),
            serde_json::to_vec(&forum).expect("forum"),
        )
        .expect("forum write");
        // export at level 0, headless, no deny tokens
        let out = root.path().join("bundle.json");
        let export = TelemetryExport {
            level: 0,
            session: Vec::new(),
            sessions_root: Some(root.path().to_path_buf()),
            redact_tokens: None,
            out: Some(out.clone()),
            yes: true,
        };
        headless(root.path(), &PricingCtx::default(), &export, None)
            .await
            .expect("the headless export should succeed");
        let bytes = std::fs::read(&out).expect("read bundle");
        let text = String::from_utf8(bytes).expect("utf8");
        // the canary never ships, in any field, at level 0
        assert!(!text.contains("CANARY"), "level 0 leaked content: {text}");
        // and no string in the document reads like prose: short, single-line
        // vocabulary only (ids, names, enums, hashes, timestamps)
        let value: serde_json::Value = serde_json::from_str(&text).expect("parse");
        walk_strings(&value, &|string| {
            assert!(
                !string.contains('\n'),
                "multi-line string shipped: {string:?}"
            );
            assert!(
                string.chars().count() <= 100,
                "overlong string shipped: {string:?}"
            );
        });
        // the folded telemetry still counted everything it should have
        let bundle = Bundle::read(&out).expect("read back");
        let session = &bundle.sessions[0];
        assert!(session.signatures.is_empty(), "level 0 ships no signatures");
        // the per-entry rows ship their classes but never a level-0 title
        assert!(!session.forum.entries.is_empty(), "entry rows must ship");
        assert!(
            session
                .forum
                .entries
                .iter()
                .all(|entry| entry.title.is_none())
        );
        assert_eq!(session.tools["run_script"].fails, 1);
        assert_eq!(session.decision_points.asks["auto_default"], 1);
        assert_eq!(session.decision_points.review_flags, 1);
        assert_eq!(session.decision_points.terminations["max_turns"], 1);
        assert_eq!(session.provider_stalls.retry_attempts, 1);
        assert!(session.provider_stalls.open_pause);
        assert_eq!(session.detectors["T1027"].confirmed, 1);
        // the agent note, the comment-question, and the two detector
        // questions all count as unsolved
        assert_eq!(session.forum.unsolved, 4);
        // the roster folded under its registry name; the sample name never
        // rides along (the no-canary sweep above already proved it)
        assert_eq!(session.detectors_run["packing"].hits, 0);
        // the forged provenance class fields gated out of the exact lead
        let lead = session
            .detector_leads
            .iter()
            .find(|lead| lead.exact)
            .expect("the exact lead");
        assert!(lead.source.is_none() && lead.artifact.is_none() && lead.detection.is_none());
    }

    /// Entry titles ship only at level 1, redacted, and drops blank them
    #[test]
    fn entry_titles_gate_by_level_and_drop() {
        use crate::handlers::ai::telemetry::bundle::{EntryStatus, ForumEntry};
        // one row with a tokened question title and a clean task description
        let mut row = SessionTelemetry::default();
        row.forum.entries = vec![
            ForumEntry {
                status: EntryStatus::Unsolved,
                title: Some("Where does secretcorp stage its payload?".to_owned()),
                techniques: vec!["T1027".to_owned(), "Capa.Rule_1".to_owned()],
                ..ForumEntry::default()
            },
            ForumEntry {
                status: EntryStatus::Unmet,
                title: Some("enumerate the exports".to_owned()),
                ..ForumEntry::default()
            },
        ];
        row.detectors
            .insert("Capa.Rule_1".to_owned(), DetectorTally::default());
        let redactor = LibraryRedactor::new(false, vec!["secretcorp".to_owned()], Vec::new())
            .expect("a redactor from one token");
        // level 0 clears every title but keeps the rows and their classes
        let level0 = finalize(
            std::slice::from_ref(&row),
            0,
            Some(&redactor),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            Utc::now(),
        );
        let entries = &level0.sessions[0].forum.entries;
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.title.is_none()));
        // level 1 redacts the surviving titles
        let level1 = finalize(
            std::slice::from_ref(&row),
            1,
            Some(&redactor),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            Utc::now(),
        );
        let entries = &level1.sessions[0].forum.entries;
        let title = entries[0].title.as_deref().expect("a level-1 title");
        assert!(title.contains("[denied]"), "got {title:?}");
        assert!(!title.contains("secretcorp"));
        // a review drop (keyed on the redacted text) blanks the title only
        let dropped = BTreeSet::from([title.to_owned()]);
        let dropped1 = finalize(
            std::slice::from_ref(&row),
            1,
            Some(&redactor),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &dropped,
            Utc::now(),
        );
        let entries = &dropped1.sessions[0].forum.entries;
        assert!(entries[0].title.is_none(), "the dropped title must blank");
        assert_eq!(entries[0].status, EntryStatus::Unsolved, "the row survives");
        assert_eq!(entries[1].title.as_deref(), Some("enumerate the exports"));
    }

    /// A dropped technique id vanishes from every position it appears in
    #[test]
    fn dropped_ids_vanish_from_every_position() {
        use crate::handlers::ai::telemetry::bundle::ForumEntry;
        let mut row = SessionTelemetry::default();
        row.detectors.insert(
            "Capa.Rule_1".to_owned(),
            DetectorTally {
                ignored: 2,
                ..DetectorTally::default()
            },
        );
        row.forum.entries = vec![ForumEntry {
            techniques: vec!["Capa.Rule_1".to_owned(), "T1027".to_owned()],
            title: None,
            ..ForumEntry::default()
        }];
        let dropped = BTreeSet::from(["Capa.Rule_1".to_owned()]);
        let bundle = finalize(
            std::slice::from_ref(&row),
            0,
            None,
            &BTreeSet::new(),
            &dropped,
            &BTreeSet::new(),
            Utc::now(),
        );
        let session = &bundle.sessions[0];
        // the tally folds under `other`, and the entry's list renames too
        assert!(!session.detectors.contains_key("Capa.Rule_1"));
        assert_eq!(session.detectors[OTHER_TECHNIQUES].ignored, 2);
        assert_eq!(session.forum.entries[0].techniques, ["T1027", "other"]);
    }

    /// Level-1 strings obey the recipe caps; only signatures are multi-line
    ///
    /// Walks a level-1 bundle built from hostile inputs: every string other
    /// than `signatures[].sig` stays single-line vocabulary, every signature
    /// obeys the 16-line / 160-per-line / 1,500-total caps, and the canary
    /// marker appears only inside signature values (level 1 ships failing
    /// result prefixes by design; level 0's canary test stays separate).
    #[tokio::test]
    async fn level_one_strings_obey_the_recipe() {
        use thorpal::{TranscriptItem, TranscriptLine};
        // one session whose failing result is a long, hostile stack trace
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("recipe-session");
        std::fs::create_dir_all(&dir).expect("session dir");
        let trace: String = (b'a'..b'a' + 26)
            .map(|letter| {
                // vary a letter per frame so lines stay distinct after the
                // digit collapse (identical neighbors would dedupe away)
                let letter = char::from(letter);
                format!("CANARY frame {letter}{letter} at /home/user/mal/strings_{letter}.rs:42\n")
            })
            .collect();
        let lines = [
            TranscriptLine {
                ts: Some(1_000),
                item: TranscriptItem::ToolCall {
                    agent: "main".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "run_script".to_owned(),
                    args: "{}".to_owned(),
                },
            },
            TranscriptLine {
                ts: Some(2_000),
                item: TranscriptItem::ToolResult {
                    agent: "main".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "run_script".to_owned(),
                    result: trace,
                    success: false,
                },
            },
        ];
        let encoded: String = lines
            .iter()
            .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
            .collect();
        std::fs::write(dir.join("transcript.jsonl"), encoded).expect("transcript");
        let out = root.path().join("bundle.json");
        let export = TelemetryExport {
            level: 1,
            session: Vec::new(),
            sessions_root: Some(root.path().to_path_buf()),
            redact_tokens: None,
            out: Some(out.clone()),
            yes: true,
        };
        headless(root.path(), &PricingCtx::default(), &export, None)
            .await
            .expect("the headless export should succeed");
        let bundle = Bundle::read(&out).expect("read back");
        // the bundle stamps the recipe it normalized with
        assert_eq!(bundle.recipe(), SIGNATURE_RECIPE);
        let session = &bundle.sessions[0];
        assert_eq!(session.signatures.len(), 1);
        let sig = &session.signatures[0].sig;
        // the signature is multi-line, path-collapsed, and capped
        assert!(sig.lines().count() > 1 && sig.lines().count() <= 16);
        assert!(sig.lines().all(|line| line.chars().count() <= 160));
        assert!(sig.chars().count() <= 1500);
        assert!(sig.contains("PATH"), "paths must collapse: {sig:?}");
        assert!(!sig.contains("/home/"), "paths must not survive: {sig:?}");
        // walk every string in the document: only signatures carry newlines
        // or the canary, and everything else stays short vocabulary
        let value: serde_json::Value =
            serde_json::from_slice(&bundle.to_bytes().expect("bytes")).expect("parse");
        let sigs: std::collections::BTreeSet<String> = bundle
            .sessions
            .iter()
            .flat_map(|session| session.signatures.iter().map(|cluster| cluster.sig.clone()))
            .collect();
        walk_strings(&value, &|string| {
            if sigs.contains(string) {
                return;
            }
            assert!(
                !string.contains('\n'),
                "multi-line non-signature: {string:?}"
            );
            assert!(
                !string.contains("CANARY"),
                "leaked outside a signature: {string:?}"
            );
            assert!(string.chars().count() <= 100, "overlong string: {string:?}");
        });
    }

    /// The explorer's local assembly and an exported bundle agree row-for-row
    ///
    /// Local mode folds the store on the fly through the same assembly the
    /// exporter runs; with no drops and no deny tokens, a level-1 export of
    /// the same store must produce identical session rows.
    #[tokio::test]
    async fn exported_rows_match_local_assembly() {
        use thorpal::{TranscriptItem, TranscriptLine};
        // the same synthetic store the round-trip test uses
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("parity-session");
        std::fs::create_dir_all(&dir).expect("session dir");
        let lines = [
            TranscriptLine {
                ts: Some(1_000),
                item: TranscriptItem::ToolCall {
                    agent: "main".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "run_script".to_owned(),
                    args: "{}".to_owned(),
                },
            },
            TranscriptLine {
                ts: Some(2_500),
                item: TranscriptItem::ToolResult {
                    agent: "main".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "run_script".to_owned(),
                    result: "script error: line 7".to_owned(),
                    success: false,
                },
            },
        ];
        let encoded: String = lines
            .iter()
            .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
            .collect();
        std::fs::write(dir.join("transcript.jsonl"), encoded).expect("transcript");
        // a detector-seeded forum so the per-entry rows flow through parity
        let forum = serde_json::json!({
            "posts": [{
                "name": "q-unpack",
                "author": "detector",
                "title": "Can the payload be unpacked?",
                "body": "See the stub.\nTechniques: T1027.",
                "kind": {"question": "unsolved"},
                "updated": 2
            }],
            "comments": [],
            "next_comment_id": 1,
            "finish_requested": false
        });
        std::fs::write(dir.join("forum.json"), forum.to_string()).expect("forum");
        let pricing = PricingCtx::default();
        // the local path: scan, forum pass, assemble -- what explore does live
        let summaries = scan_once(root.path(), &pricing).await;
        assert_eq!(summaries.len(), 1);
        let outcomes = crate::handlers::ai::telemetry::fold::read_forum(&dir).await;
        let local = crate::handlers::ai::telemetry::fold::assemble_session(&summaries[0], outcomes);
        // the export path: the real headless pipeline at level 1, no drops
        let out = root.path().join("parity.json");
        let export = TelemetryExport {
            level: 1,
            session: Vec::new(),
            sessions_root: Some(root.path().to_path_buf()),
            redact_tokens: None,
            out: Some(out.clone()),
            yes: true,
        };
        headless(root.path(), &pricing, &export, None)
            .await
            .expect("the headless export should succeed");
        let exported = Bundle::read(&out).expect("read back");
        // the rows agree exactly, field for field
        let local_value = serde_json::to_value(&local).expect("local row");
        let exported_value = serde_json::to_value(&exported.sessions[0]).expect("exported row");
        assert_eq!(local_value, exported_value);
        // and the parity covered a populated entry row, not an empty list
        assert_eq!(exported.sessions[0].forum.entries.len(), 1);
        assert!(exported.sessions[0].forum.entries[0].title.is_some());
    }
}
