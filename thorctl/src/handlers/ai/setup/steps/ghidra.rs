//! The Ghidra page: pick a detected install, enter one, or download a release

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Gauge, Paragraph, Wrap};

use crate::handlers::ai::chat::TextBox;

use super::super::app::WizardState;
use super::super::ghidra::validate_install;
use super::{dim_style, idle_style, pointer, selected_style, spinner};

/// What a key press on the Ghidra page asks the app to do
pub enum GhidraOutcome {
    /// Nothing; the page consumed the key itself
    None,
    /// Use this validated install
    Chosen(PathBuf),
    /// Download a release into the install root
    Download,
    /// Extract this local release zip into the install root
    LocalZip(PathBuf),
    /// Leave the Ghidra install unconfigured for now
    Skip,
    /// Go back one step
    Back,
    /// Cancel the in-flight install pipeline
    Cancel,
    /// Retry the failed install pipeline
    Retry,
}

/// One selectable row on the Ghidra page
enum GhidraRow {
    /// A validated install found on this box
    Install(PathBuf),
    /// Open the manual path entry box
    Manual,
    /// Download a release from GitHub
    Download,
    /// Extract the zip passed on the command line
    LocalZip(PathBuf),
    /// Leave the install unconfigured
    Skip,
}

/// Which face of the Ghidra page is showing
pub enum GhidraMode {
    /// The selection list
    List,
    /// The manual path entry box
    Manual,
    /// The install pipeline is running
    Installing(InstallPhase),
    /// The install pipeline failed
    Failed(String),
}

/// How far the running install pipeline has progressed
pub enum InstallPhase {
    /// Resolving the release from the GitHub API
    Resolving,
    /// Streaming the release zip down
    Downloading {
        /// The bytes written so far
        bytes: u64,
        /// The total size when the response named one
        total: Option<u64>,
    },
    /// Extracting the release zip
    Extracting {
        /// The archive entries extracted so far
        done: usize,
        /// The archive's total entry count
        total: usize,
    },
}

/// The Ghidra page's state
pub struct GhidraStep {
    /// Which face of the page is showing
    pub mode: GhidraMode,
    /// The selected list row
    selected: usize,
    /// The manual path entry box
    manual: TextBox,
    /// The resolved release's tag and asset name, once known
    release: Option<(String, String)>,
    /// The last manual-path validation error
    pub error: Option<String>,
}

impl GhidraStep {
    /// Create the Ghidra page on its selection list
    pub fn new() -> Self {
        GhidraStep {
            mode: GhidraMode::List,
            selected: 0,
            manual: TextBox::default()
                .title("Ghidra install directory")
                .placeholder("/path/to/ghidra_12.1.4_PUBLIC"),
            release: None,
            error: None,
        }
    }

    /// The selectable rows, derived fresh from the wizard's shared state
    ///
    /// # Arguments
    ///
    /// * `state` - The wizard's shared state
    fn rows(&self, state: &WizardState) -> Vec<GhidraRow> {
        let mut rows = Vec::new();
        // a previously chosen install (e.g. a manual path) leads the list
        if let Some(chosen) = &state.plan.ghidra_install {
            rows.push(GhidraRow::Install(chosen.clone()));
        }
        // then every detected install not already listed
        if let Some(detected) = &state.detected {
            for install in &detected.installs {
                if state.plan.ghidra_install.as_ref() != Some(install) {
                    rows.push(GhidraRow::Install(install.clone()));
                }
            }
        }
        // then the manual, download, offline-zip, and skip actions
        rows.push(GhidraRow::Manual);
        rows.push(GhidraRow::Download);
        if let Some(zip) = &state.ghidra_zip {
            rows.push(GhidraRow::LocalZip(zip.clone()));
        }
        rows.push(GhidraRow::Skip);
        rows
    }

    /// Handle a key press on the Ghidra page
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `state` - The wizard's shared state
    pub fn handle_key(&mut self, key: KeyEvent, state: &WizardState) -> GhidraOutcome {
        match &self.mode {
            GhidraMode::List => {
                // the row set can change as detection lands; stay in bounds
                let rows = self.rows(state);
                self.selected = self.selected.min(rows.len().saturating_sub(1));
                match key.code {
                    // escape backs out of the page
                    KeyCode::Esc => GhidraOutcome::Back,
                    // vim keys and arrows walk the rows with wraparound
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.selected = (self.selected + 1) % rows.len();
                        GhidraOutcome::None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.selected = (self.selected + rows.len() - 1) % rows.len();
                        GhidraOutcome::None
                    }
                    // s skips without walking to the skip row
                    KeyCode::Char('s') => GhidraOutcome::Skip,
                    // enter picks the selected row
                    KeyCode::Enter => match &rows[self.selected] {
                        GhidraRow::Install(install) => GhidraOutcome::Chosen(install.clone()),
                        GhidraRow::Manual => {
                            // open the path box prefilled with the last entry
                            self.mode = GhidraMode::Manual;
                            self.error = None;
                            GhidraOutcome::None
                        }
                        GhidraRow::Download => GhidraOutcome::Download,
                        GhidraRow::LocalZip(zip) => GhidraOutcome::LocalZip(zip.clone()),
                        GhidraRow::Skip => GhidraOutcome::Skip,
                    },
                    _ => GhidraOutcome::None,
                }
            }
            GhidraMode::Manual => {
                // escape closes the path box back to the list
                if key.code == KeyCode::Esc {
                    self.mode = GhidraMode::List;
                    self.error = None;
                    return GhidraOutcome::None;
                }
                // control chords never type into the path box (ctrl+j would
                // embed a newline mid-path; ctrl+c quits globally)
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return GhidraOutcome::None;
                }
                // route the key into the box; enter submits a non-empty path
                if self.manual.handle_key(key, false) {
                    let path = PathBuf::from(self.manual.content.trim());
                    // only a directory carrying analyzeHeadless is accepted
                    if validate_install(&path) {
                        self.error = None;
                        return GhidraOutcome::Chosen(path);
                    }
                    self.error = Some(format!(
                        "'{}' is not a Ghidra install (no support/analyzeHeadless)",
                        path.display()
                    ));
                }
                GhidraOutcome::None
            }
            GhidraMode::Installing(_) => match key.code {
                // escape cancels the pipeline and returns to the list
                KeyCode::Esc => GhidraOutcome::Cancel,
                _ => GhidraOutcome::None,
            },
            GhidraMode::Failed(_) => match key.code {
                // retry re-runs the same source; escape returns to the list
                KeyCode::Char('r') => GhidraOutcome::Retry,
                KeyCode::Esc => {
                    self.mode = GhidraMode::List;
                    GhidraOutcome::None
                }
                _ => GhidraOutcome::None,
            },
        }
    }

    /// Insert pasted text into the manual path box
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub fn paste(&mut self, text: &str) {
        // pastes only make sense while the path box is open
        if matches!(self.mode, GhidraMode::Manual) {
            self.manual.insert_str(text.trim_end_matches(['\r', '\n']));
        }
    }

    /// Fold the resolved release into the page's install labels
    ///
    /// # Arguments
    ///
    /// * `tag` - The resolved release's git tag
    /// * `name` - The resolved asset's file name
    pub fn on_resolved(&mut self, tag: String, name: String) {
        self.release = Some((tag, name));
        self.mode = GhidraMode::Installing(InstallPhase::Downloading {
            bytes: 0,
            total: None,
        });
    }

    /// Fold a download progress reading into the page
    ///
    /// # Arguments
    ///
    /// * `bytes` - The bytes written so far
    /// * `total` - The total size when the response named one
    pub fn on_download(&mut self, bytes: u64, total: Option<u64>) {
        self.mode = GhidraMode::Installing(InstallPhase::Downloading { bytes, total });
    }

    /// Fold an extraction progress reading into the page
    ///
    /// # Arguments
    ///
    /// * `done` - The archive entries extracted so far
    /// * `total` - The archive's total entry count
    pub fn on_extract(&mut self, done: usize, total: usize) {
        self.mode = GhidraMode::Installing(InstallPhase::Extracting { done, total });
    }

    /// Fold an install failure into the page
    ///
    /// # Arguments
    ///
    /// * `error` - The pipeline's human-readable failure
    pub fn on_failed(&mut self, error: String) {
        self.mode = GhidraMode::Failed(error);
    }

    /// Reset the page back to its selection list
    pub fn reset(&mut self) {
        self.mode = GhidraMode::List;
        self.release = None;
    }

    /// Render the Ghidra page
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    /// * `state` - The wizard's shared state
    pub fn render(&mut self, frame: &mut Frame, area: Rect, state: &WizardState) {
        match &self.mode {
            GhidraMode::List => self.render_list(frame, area, state),
            GhidraMode::Manual => self.render_manual(frame, area),
            GhidraMode::Installing(phase) => self.render_installing(frame, area, phase, state),
            GhidraMode::Failed(error) => render_failed(frame, area, error),
        }
    }

    /// Render the selection list
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    /// * `state` - The wizard's shared state
    fn render_list(&mut self, frame: &mut Frame, area: Rect, state: &WizardState) {
        let rows = self.rows(state);
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        let mut lines = vec![
            Line::styled(
                "ThorPal drives Ghidra's analyzeHeadless for binary analysis. Pick a \
                detected install, enter a path, or download a release.",
                dim_style(),
            ),
            Line::raw(""),
        ];
        // a still-running scan is said out loud instead of looking empty
        if state.detected.is_none() {
            lines.push(Line::styled(
                format!("{} scanning this box for installs…", spinner()),
                dim_style(),
            ));
            lines.push(Line::raw(""));
        }
        for (index, row) in rows.iter().enumerate() {
            let selected = index == self.selected;
            let style = if selected {
                selected_style()
            } else {
                idle_style()
            };
            let line = match row {
                GhidraRow::Install(install) => Line::from(vec![
                    pointer(selected),
                    Span::styled(format!("use {}", install.display()), style),
                    Span::styled("  (valid install)", Style::default().fg(Color::Green)),
                ]),
                GhidraRow::Manual => Line::from(vec![
                    pointer(selected),
                    Span::styled("enter an install path manually".to_string(), style),
                ]),
                GhidraRow::Download => {
                    // name the pinned release when one was passed
                    let label = match &state.ghidra_version {
                        Some(version) => format!("download Ghidra {version} from GitHub"),
                        None => "download the latest Ghidra release from GitHub".to_string(),
                    };
                    Line::from(vec![
                        pointer(selected),
                        Span::styled(label, style),
                        Span::styled(
                            format!("  (into {})", state.install_root.display()),
                            dim_style(),
                        ),
                    ])
                }
                GhidraRow::LocalZip(zip) => Line::from(vec![
                    pointer(selected),
                    Span::styled(format!("install from {}", zip.display()), style),
                    Span::styled("  (offline zip)", dim_style()),
                ]),
                GhidraRow::Skip => Line::from(vec![
                    pointer(selected),
                    Span::styled("skip — configure Ghidra later".to_string(), style),
                    Span::styled(
                        "  (agents that use Ghidra tools will refuse to start)",
                        dim_style(),
                    ),
                ]),
            };
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    /// Render the manual path entry box
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    fn render_manual(&mut self, frame: &mut Frame, area: Rect) {
        // carve an intro, the path box, and an error strip
        let [intro, boxed, status] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Enter the Ghidra install directory (the one containing \
                support/analyzeHeadless). Enter validates, esc goes back.",
                dim_style(),
            ))
            .wrap(Wrap { trim: false }),
            intro,
        );
        self.manual.is_focused = true;
        self.manual.render(frame, boxed);
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(Line::styled(error.clone(), Style::default().fg(Color::Red)))
                    .wrap(Wrap { trim: false }),
                status,
            );
        }
    }

    /// Render the running install pipeline
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    /// * `phase` - The pipeline's current phase
    /// * `state` - The wizard's shared state
    fn render_installing(
        &self,
        frame: &mut Frame,
        area: Rect,
        phase: &InstallPhase,
        state: &WizardState,
    ) {
        // carve a header, the progress gauge, and a hint strip
        let [header, gauge, hint] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .areas(area);
        // the header names what is installing and where
        let what = match &self.release {
            Some((tag, name)) => format!("{tag} ({name})"),
            None => "Ghidra".to_string(),
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    format!("Installing {what}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    format!("into {}", state.install_root.display()),
                    dim_style(),
                ),
            ]),
            header,
        );
        // the gauge reflects the live phase
        match phase {
            InstallPhase::Resolving => {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        format!("{} resolving the release from api.github.com…", spinner()),
                        Style::default().fg(Color::Yellow),
                    )),
                    gauge,
                );
            }
            InstallPhase::Downloading { bytes, total } => match total {
                // a known total renders a real progress gauge
                Some(total) if *total > 0 => {
                    let ratio = (*bytes as f64 / *total as f64).clamp(0.0, 1.0);
                    frame.render_widget(
                        Gauge::default()
                            .ratio(ratio)
                            .gauge_style(Style::default().fg(Color::Cyan))
                            .label(format!(
                                "downloading {} / {}",
                                fmt_bytes(*bytes),
                                fmt_bytes(*total)
                            )),
                        gauge,
                    );
                }
                // an unsized response still shows the moving byte count
                _ => {
                    frame.render_widget(
                        Paragraph::new(Line::styled(
                            format!("{} downloading… {}", spinner(), fmt_bytes(*bytes)),
                            Style::default().fg(Color::Yellow),
                        )),
                        gauge,
                    );
                }
            },
            InstallPhase::Extracting { done, total } => {
                let ratio = if *total > 0 {
                    (*done as f64 / *total as f64).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                frame.render_widget(
                    Gauge::default()
                        .ratio(ratio)
                        .gauge_style(Style::default().fg(Color::Cyan))
                        .label(format!("extracting {done} / {total} files")),
                    gauge,
                );
            }
        }
        frame.render_widget(
            Paragraph::new(Line::styled("esc cancels the install", dim_style())),
            hint,
        );
    }
}

/// Render the failed install pipeline
///
/// # Arguments
///
/// * `frame` - The frame to render into
/// * `area` - The body area to render in
/// * `error` - The pipeline's human-readable failure
fn render_failed(frame: &mut Frame, area: Rect, error: &str) {
    let mut lines = vec![
        Line::styled(
            "✗ the Ghidra install failed",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    // the pipeline's error chain explains what actually broke
    for part in error.lines() {
        lines.push(Line::styled(
            part.to_string(),
            Style::default().fg(Color::Red),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Press r to retry, or esc to go back to the list.",
        dim_style(),
    ));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Format a byte count as a human-readable size
///
/// # Arguments
///
/// * `bytes` - The byte count to format
fn fmt_bytes(bytes: u64) -> String {
    // scale into the conventional binary units with one decimal
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
