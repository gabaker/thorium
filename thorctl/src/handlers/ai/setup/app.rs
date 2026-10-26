//! The setup wizard shell: a linear step machine over the shared TUI loop
//!
//! Mirrors the usage TUI's loop shape -- a kanal event channel fed by the
//! shared terminal/SIGTERM forwarders, burst-drained into a single draw per
//! wakeup -- with one cancellable background task at a time (login, provider
//! check, the Ghidra install pipeline, the JDK probe) guarded by a generation
//! counter so a cancelled task's late results always drop at the loop.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use kanal::{AsyncReceiver, AsyncSender};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use tokio::task::JoinHandle;

use thorium::client::conf::{AiProviderKind, ResolvedAiProvider};
use thorium::{CtlConf, Error, Keys, Thorium};

use crate::args::ai::Setup as SetupArgs;
use crate::handlers::ai::batch::scan::disk_free_gib;
use crate::handlers::ai::session;

use super::config::{self, SetupPlan};
use super::ghidra::{self, InstallSource};
use super::jdk::{self, JdkReport};
use super::steps::credentials::{CredentialsStep, CredsOutcome};
use super::steps::ghidra::{GhidraMode, GhidraOutcome, GhidraStep, InstallPhase};
use super::steps::login::{LoginOutcome, LoginStep};
use super::steps::projects::{ProjectsOutcome, ProjectsStep};
use super::steps::provider::{ProviderOutcome, ProviderStep};
use super::steps::{java, review, verify, welcome};

/// The trivial prompt the test-connection step sends, matching `analyze check`
const CHECK_PROMPT: &str = "hello";

/// One event the setup wizard's loop handles
pub enum SetupEvent {
    /// A terminal event (keyboard, paste, resize, etc.)
    Terminal(Event),
    /// Quit the TUI gracefully (e.g. a SIGTERM asked the process to stop)
    Quit,
    /// Redraw the tui so spinners and gauges animate
    Redraw,
    /// The startup environment scan finished; never generation-guarded since
    /// the scan is read-only context that stays true across steps
    DetectDone(Detected),
    /// An in-wizard Thorium login succeeded
    LoginDone {
        /// The task generation this result belongs to
        generation: u64,
        /// The keys the login produced
        keys: Keys,
        /// The username that logged in
        username: String,
    },
    /// An in-wizard Thorium login failed
    LoginFailed {
        /// The task generation this result belongs to
        generation: u64,
        /// The human-readable login failure
        error: String,
    },
    /// The provider check round trip succeeded
    CheckDone {
        /// The task generation this result belongs to
        generation: u64,
        /// The model's trimmed reply, as proof of the round trip
        reply: String,
    },
    /// The provider check round trip failed
    CheckFailed {
        /// The task generation this result belongs to
        generation: u64,
        /// The provider's full error chain
        error: String,
    },
    /// The install pipeline resolved its release
    ReleaseResolved {
        /// The task generation this result belongs to
        generation: u64,
        /// The release's git tag
        tag: String,
        /// The release asset's file name
        name: String,
        /// The release asset's size in bytes
        size: u64,
    },
    /// The install pipeline streamed more of the release down
    DownloadProgress {
        /// The task generation this result belongs to
        generation: u64,
        /// The bytes written so far
        bytes: u64,
        /// The total size when the response named one
        total: Option<u64>,
    },
    /// The install pipeline extracted more archive entries
    ExtractProgress {
        /// The task generation this result belongs to
        generation: u64,
        /// The archive entries extracted so far
        done: usize,
        /// The archive's total entry count
        total: usize,
    },
    /// The install pipeline finished with a validated install
    ExtractDone {
        /// The task generation this result belongs to
        generation: u64,
        /// The validated install the pipeline produced
        install: PathBuf,
    },
    /// The install pipeline failed
    GhidraFailed {
        /// The task generation this result belongs to
        generation: u64,
        /// The pipeline's full error chain
        error: String,
    },
    /// The JDK probe finished
    JdkChecked {
        /// The task generation this result belongs to
        generation: u64,
        /// The probe's outcome
        report: JdkReport,
    },
}

/// The read-only environment scan shown on the welcome page
pub struct Detected {
    /// The valid Ghidra installs found on this box
    pub installs: Vec<PathBuf>,
    /// The JDK probe against the documented default floor
    pub jdk: JdkReport,
}

/// The wizard's steps in flow order
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The detected-state summary
    Welcome,
    /// The optional Thorium login (fresh boxes only)
    Login,
    /// The LLM provider selection
    Provider,
    /// The per-provider credentials form
    Credentials,
    /// The optional live provider check
    Verify,
    /// The Ghidra install selection/download
    Ghidra,
    /// The JDK probe for the chosen install
    Jdk,
    /// The Ghidra projects directory
    Projects,
    /// The final review before anything writes
    Review,
    /// The post-save summary
    Done,
}

impl Step {
    /// The breadcrumb label for this step
    fn label(self) -> &'static str {
        match self {
            Step::Welcome => "welcome",
            Step::Login => "login",
            Step::Provider => "provider",
            Step::Credentials => "credentials",
            Step::Verify => "test",
            Step::Ghidra => "ghidra",
            Step::Jdk => "java",
            Step::Projects => "projects",
            Step::Review => "review",
            Step::Done => "done",
        }
    }

    /// This step's position in the flow, for done/pending breadcrumb styling
    fn order(self) -> usize {
        match self {
            Step::Welcome => 0,
            Step::Login => 1,
            Step::Provider => 2,
            Step::Credentials => 3,
            Step::Verify => 4,
            Step::Ghidra => 5,
            Step::Jdk => 6,
            Step::Projects => 7,
            Step::Review => 8,
            Step::Done => 9,
        }
    }
}

/// The shared wizard data the steps read and write
pub struct WizardState {
    /// The config path the wizard reads and writes
    pub config_path: PathBuf,
    /// The config loaded at startup, when one parsed
    pub existing: Option<CtlConf>,
    /// The parse error when a config file existed but would not load
    pub config_error: Option<String>,
    /// The wizard's drafted answers
    pub plan: SetupPlan,
    /// The background environment scan, once it lands
    pub detected: Option<Detected>,
    /// The JDK report for the chosen install, once probed
    pub jdk: Option<JdkReport>,
    /// Whether a JDK probe is in flight
    pub jdk_checking: bool,
    /// The provider check's outcome, once one ran
    pub check: Option<Result<String, String>>,
    /// Whether a provider check is in flight
    pub checking: bool,
    /// Whether a Thorium login is in flight
    pub logging_in: bool,
    /// The username an in-wizard login authenticated as
    pub logged_in_as: Option<String>,
    /// Whether the user skipped the login step on a fresh box
    pub login_skipped: bool,
    /// The root downloaded Ghidra releases are installed under
    pub install_root: PathBuf,
    /// The pinned Ghidra release to download, when one was passed
    pub ghidra_version: Option<String>,
    /// The local Ghidra zip to install from, when one was passed
    pub ghidra_zip: Option<PathBuf>,
    /// The shared default Ghidra projects dir
    pub default_projects: PathBuf,
}

/// Sends a redraw four times a second so spinners and gauges animate
///
/// # Arguments
///
/// * `sender` - The channel to request redraws over
async fn ticker(sender: AsyncSender<SetupEvent>) {
    // keep ticking for the life of the app
    loop {
        // sleep between frames; the spinner frames advance every 100ms but a
        // wizard tolerates a slightly chunkier animation for a quarter of the
        // wakeups
        tokio::time::sleep(Duration::from_millis(250)).await;
        // try to send a redraw event; a failure just means the app is gone
        if sender.send(SetupEvent::Redraw).await.is_err() {
            return;
        }
    }
}

/// Log into Thorium and mint the token keys the config saves
///
/// # Arguments
///
/// * `url` - The Thorium API url to log into
/// * `username` - The username to authenticate as
/// * `password` - The password to authenticate with
async fn login(url: String, username: String, password: String) -> Result<(Keys, String), Error> {
    // build a default-settings client and basic-auth into Thorium; users with
    // self-signed certs are pointed at `thorctl login --invalid-certs` instead
    let thorium = Thorium::build(&url)
        .basic_auth(username, password)
        .build()
        .await?;
    // the fresh token comes back on the user's own info
    let user = thorium.users.info().await?;
    Ok((Keys::new_token(&url, user.token), user.username))
}

/// The setup wizard's application state
pub struct SetupApp {
    /// The sender feeding the app's own event loop
    event_tx: AsyncSender<SetupEvent>,
    /// The receiver the app's event loop drains
    event_rx: AsyncReceiver<SetupEvent>,
    /// The shared wizard data the steps read and write
    state: WizardState,
    /// The step currently showing
    step: Step,
    /// The login page's input state
    login: LoginStep,
    /// The provider page's selection state
    provider: ProviderStep,
    /// The credentials form, built when a provider is chosen
    credentials: Option<CredentialsStep>,
    /// The Ghidra page's state
    ghidra: GhidraStep,
    /// The projects page's input state
    projects: ProjectsStep,
    /// The install source behind the current/last pipeline, for retries
    install_source: Option<InstallSource>,
    /// The one cancellable background task, when one is running
    task: Option<JoinHandle<()>>,
    /// The generation guard stale task results are dropped against
    generation: u64,
    /// The in-flight download's partial file, removed on cancel
    part_file: Option<PathBuf>,
    /// The last footer error, cleared on the next key press
    error: Option<String>,
    /// Whether the next `q` on the review page quits without writing
    confirm_quit: bool,
    /// Set once the user asked to quit
    should_quit: bool,
}

impl SetupApp {
    /// Creates the wizard app over the loaded config and the setup flags
    ///
    /// # Arguments
    ///
    /// * `config_path` - The config path the wizard reads and writes
    /// * `existing` - The config loaded at startup, when one parsed
    /// * `config_error` - The parse error when a config existed but would not load
    /// * `install_root` - The root downloaded releases are installed under
    /// * `default_projects` - The shared default Ghidra projects dir
    /// * `cmd` - The parsed `setup` command flags
    pub fn new(
        config_path: PathBuf,
        existing: Option<CtlConf>,
        config_error: Option<String>,
        install_root: PathBuf,
        default_projects: PathBuf,
        cmd: &SetupArgs,
    ) -> Self {
        // the app's own event channel, shared with every background task
        let (event_tx, event_rx) = kanal::unbounded_async();
        // preselect the provider the loaded config already resolves to
        let active = existing
            .as_ref()
            .and_then(|conf| conf.ai.as_ref())
            .and_then(|ai| ai.resolve().ok())
            .map(|resolved| kind_of(&resolved));
        // prefill the projects page with the configured dir or the default
        let projects_prefill = existing
            .as_ref()
            .and_then(|conf| conf.ai.as_ref())
            .and_then(|ai| ai.ghidra_projects.clone())
            .unwrap_or_else(|| default_projects.clone());
        let state = WizardState {
            config_path,
            existing,
            config_error,
            plan: SetupPlan::default(),
            detected: None,
            jdk: None,
            jdk_checking: false,
            check: None,
            checking: false,
            logging_in: false,
            logged_in_as: None,
            login_skipped: false,
            install_root,
            ghidra_version: cmd.ghidra_version.clone(),
            ghidra_zip: cmd.ghidra_zip.clone(),
            default_projects,
        };
        SetupApp {
            event_tx,
            event_rx,
            provider: ProviderStep::new(active),
            projects: ProjectsStep::new(projects_prefill.display().to_string()),
            state,
            step: Step::Welcome,
            login: LoginStep::new(),
            credentials: None,
            ghidra: GhidraStep::new(),
            install_source: None,
            task: None,
            generation: 0,
            part_file: None,
            error: None,
            confirm_quit: false,
            should_quit: false,
        }
    }

    /// Whether the wizard offers the Thorium login step this run
    fn login_needed(&self) -> bool {
        // the login step exists exactly when no config parsed at startup
        self.state.existing.is_none()
    }

    /// Runs the app's event loop until the user quits
    ///
    /// # Arguments
    ///
    /// * `terminal` - The terminal to render frames into
    pub async fn start(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        // spawn the shared terminal and SIGTERM forwarders
        super::super::tui::spawn_forwarders(&self.event_tx, SetupEvent::Terminal, || {
            SetupEvent::Quit
        });
        // spawn the read-only environment scan feeding the welcome page; it
        // stays valid across steps, so it never rides the cancellable slot
        let events = self.event_tx.clone();
        let roots = ghidra::candidate_roots(
            self.state
                .existing
                .as_ref()
                .and_then(|conf| conf.ai.as_ref())
                .and_then(|ai| ai.ghidra_install.as_deref()),
            &self.state.install_root,
        );
        tokio::spawn(async move {
            // the scan is blocking fs/subprocess work, so keep it off the loop
            let installs =
                tokio::task::spawn_blocking(move || ghidra::detect_installs(&roots)).await;
            let jdk = jdk::check_jdk(None).await;
            let detected = Detected {
                installs: installs.unwrap_or_default(),
                jdk,
            };
            let _ = events.send(SetupEvent::DetectDone(detected)).await;
        });
        // spawn the ticker that animates spinners and gauges
        tokio::spawn(ticker(self.event_tx.clone()));
        // draw an initial frame until we get an event to handle
        terminal.draw(|frame| self.render(frame))?;
        // a non-blocking handle to the same event channel, used to coalesce a
        // burst of immediately-pending events into a single draw below
        let drain_rx = self.event_rx.clone_sync();
        while !self.should_quit {
            // get the next event to handle, blocking until one arrives
            match self.event_rx.recv().await {
                Ok(event) => {
                    // handle this event, then greedily drain any further
                    // events already queued so a flood of download progress
                    // collapses into one draw rather than one per event
                    self.handle_event(event);
                    while let Ok(Some(next)) = drain_rx.try_recv() {
                        // stop draining once asked to quit; the loop exits
                        if self.should_quit {
                            break;
                        }
                        self.handle_event(next);
                    }
                    // draw a single frame reflecting all the events handled
                    terminal.draw(|frame| self.render(frame))?;
                }
                Err(_) => {
                    // the channel closed; exit
                    break;
                }
            }
        }
        // a quit mid-pipeline still cancels the task and removes partials
        self.cancel_task();
        Ok(())
    }

    /// Handles one event off the loop
    ///
    /// # Arguments
    ///
    /// * `event` - The event to handle
    fn handle_event(&mut self, event: SetupEvent) {
        match event {
            // keys route by the showing step
            SetupEvent::Terminal(Event::Key(key)) => {
                // ignore key releases/repeat chatter on terminals that send it
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key);
                }
            }
            // pastes land in whichever text box the showing step focuses
            SetupEvent::Terminal(Event::Paste(text)) => match self.step {
                Step::Login => self.login.paste(&text),
                Step::Credentials => {
                    if let Some(form) = &mut self.credentials {
                        form.paste(&text);
                    }
                }
                Step::Ghidra => self.ghidra.paste(&text),
                Step::Projects => self.projects.paste(&text),
                _ => {}
            },
            // resizes and mouse events just fall through to the coming redraw
            SetupEvent::Terminal(_) => {}
            // the environment scan stays valid for the whole run
            SetupEvent::DetectDone(detected) => self.state.detected = Some(detected),
            // a login finished; a stale generation means it was cancelled
            SetupEvent::LoginDone {
                generation,
                keys,
                username,
            } => {
                if generation == self.generation {
                    self.task = None;
                    self.state.logging_in = false;
                    self.login.error = None;
                    self.state.plan.keys = Some(keys);
                    self.state.logged_in_as = Some(username);
                    // a login after an earlier skip supersedes the skip
                    self.state.login_skipped = false;
                    self.goto(Step::Provider);
                }
            }
            SetupEvent::LoginFailed { generation, error } => {
                if generation == self.generation {
                    self.task = None;
                    self.state.logging_in = false;
                    // self-signed Thorium servers need the login command's TLS
                    // flags, which the wizard deliberately does not reproduce
                    let hint = if error.to_lowercase().contains("certificate") {
                        " (for self-signed servers run `thorctl login --invalid-certs` instead)"
                    } else {
                        ""
                    };
                    self.login.error = Some(format!("{error}{hint}"));
                }
            }
            // a provider check finished
            SetupEvent::CheckDone { generation, reply } => {
                if generation == self.generation {
                    self.task = None;
                    self.state.checking = false;
                    self.state.check = Some(Ok(reply));
                }
            }
            SetupEvent::CheckFailed { generation, error } => {
                if generation == self.generation {
                    self.task = None;
                    self.state.checking = false;
                    self.state.check = Some(Err(error));
                }
            }
            // the install pipeline resolved its release: remember the partial
            // file for cancel cleanup and soft-warn when disk looks tight
            SetupEvent::ReleaseResolved {
                generation,
                tag,
                name,
                size,
            } => {
                if generation == self.generation {
                    self.part_file = Some(self.state.install_root.join(format!(".{name}.part")));
                    self.warn_on_low_disk(size);
                    self.ghidra.on_resolved(tag, name);
                }
            }
            SetupEvent::DownloadProgress {
                generation,
                bytes,
                total,
            } => {
                if generation == self.generation {
                    self.ghidra.on_download(bytes, total);
                }
            }
            SetupEvent::ExtractProgress {
                generation,
                done,
                total,
            } => {
                if generation == self.generation {
                    self.ghidra.on_extract(done, total);
                }
            }
            // the pipeline finished with a validated install
            SetupEvent::ExtractDone {
                generation,
                install,
            } => {
                if generation == self.generation {
                    self.task = None;
                    self.part_file = None;
                    self.state.plan.ghidra_install = Some(install);
                    self.ghidra.reset();
                    self.goto(Step::Jdk);
                }
            }
            SetupEvent::GhidraFailed { generation, error } => {
                if generation == self.generation {
                    self.task = None;
                    // a failed download's partial file is useless; drop it
                    if let Some(part) = self.part_file.take() {
                        let _ = std::fs::remove_file(part);
                    }
                    self.ghidra.on_failed(error);
                }
            }
            // the JDK probe finished
            SetupEvent::JdkChecked { generation, report } => {
                if generation == self.generation {
                    self.task = None;
                    self.state.jdk_checking = false;
                    self.state.jdk = Some(report);
                }
            }
            // quit gracefully
            SetupEvent::Quit => self.should_quit = true,
            // a redraw happens after every handled event anyway
            SetupEvent::Redraw => {}
        }
    }

    /// Handles a key press by the showing step
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    fn handle_key(&mut self, key: KeyEvent) {
        // any action clears a stale footer error and disarms the quit confirm
        self.error = None;
        let confirm_armed = std::mem::take(&mut self.confirm_quit);
        // ctrl-c always quits without writing, mid-task or not
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.step {
            Step::Welcome => match key.code {
                // begin the flow at the login page or straight at providers
                KeyCode::Enter => {
                    if self.login_needed() {
                        self.goto(Step::Login);
                    } else {
                        self.goto(Step::Provider);
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
                _ => {}
            },
            Step::Login => {
                // a login in flight owns the page; escape cancels it
                if self.state.logging_in {
                    if key.code == KeyCode::Esc {
                        self.cancel_task();
                        self.state.logging_in = false;
                    }
                    return;
                }
                match self.login.handle_key(key) {
                    LoginOutcome::Submit {
                        url,
                        username,
                        password,
                    } => self.spawn_login(url, username, password),
                    LoginOutcome::Skip => {
                        // skipping genuinely discards an earlier login too, so
                        // the saved config really gets placeholder keys
                        self.state.login_skipped = true;
                        self.state.plan.keys = None;
                        self.state.logged_in_as = None;
                        self.goto(Step::Provider);
                    }
                    LoginOutcome::Back => self.goto(Step::Welcome),
                    LoginOutcome::None => {}
                }
            }
            Step::Provider => match self.provider.handle_key(key) {
                ProviderOutcome::Chosen(kind) => {
                    // build the form prefilled from the loaded config's block
                    let ai = self
                        .state
                        .existing
                        .as_ref()
                        .and_then(|conf| conf.ai.as_ref());
                    self.credentials = Some(CredentialsStep::new(kind, ai));
                    self.goto(Step::Credentials);
                }
                ProviderOutcome::Skip => {
                    // skip keeps the config's provider exactly as it is, so an
                    // earlier draft (and its check result) is discarded
                    self.state.plan.provider = None;
                    self.state.check = None;
                    self.goto(Step::Ghidra);
                }
                ProviderOutcome::Back => {
                    if self.login_needed() {
                        self.goto(Step::Login);
                    } else {
                        self.goto(Step::Welcome);
                    }
                }
                ProviderOutcome::None => {}
            },
            Step::Credentials => {
                let Some(form) = &mut self.credentials else {
                    // a missing form means the step was reached out of order
                    self.goto(Step::Provider);
                    return;
                };
                match form.handle_key(key) {
                    CredsOutcome::Submit(resolved) => {
                        // a fresh draft invalidates any earlier check result
                        self.state.plan.provider = Some(*resolved);
                        self.state.check = None;
                        self.goto(Step::Verify);
                    }
                    CredsOutcome::Back => self.goto(Step::Provider),
                    CredsOutcome::None => {}
                }
            }
            Step::Verify => {
                // a check in flight owns the page; escape cancels it
                if self.state.checking {
                    if key.code == KeyCode::Esc {
                        self.cancel_task();
                        self.state.checking = false;
                    }
                    return;
                }
                match key.code {
                    KeyCode::Char('t') => self.spawn_check(),
                    KeyCode::Char('s') | KeyCode::Enter => self.goto(Step::Ghidra),
                    KeyCode::Esc => self.goto(Step::Credentials),
                    _ => {}
                }
            }
            Step::Ghidra => match self.ghidra.handle_key(key, &self.state) {
                GhidraOutcome::Chosen(install) => {
                    self.state.plan.ghidra_install = Some(install);
                    self.ghidra.reset();
                    self.goto(Step::Jdk);
                }
                GhidraOutcome::Download => {
                    let version = self.state.ghidra_version.clone();
                    self.spawn_install(InstallSource::Download { version });
                }
                GhidraOutcome::LocalZip(zip) => {
                    self.spawn_install(InstallSource::LocalZip(zip));
                }
                GhidraOutcome::Skip => {
                    // a skipped install also skips the install-specific jdk page
                    self.state.plan.ghidra_install = None;
                    self.goto(Step::Projects);
                }
                GhidraOutcome::Back => {
                    // back lands on the test page only when a provider was drafted
                    if self.state.plan.provider.is_some() {
                        self.goto(Step::Verify);
                    } else {
                        self.goto(Step::Provider);
                    }
                }
                GhidraOutcome::Cancel => {
                    // kill the pipeline, drop its partial file, back to the list
                    self.cancel_task();
                    self.ghidra.reset();
                }
                GhidraOutcome::Retry => {
                    // re-run the same source the failed pipeline used
                    if let Some(source) = self.install_source.clone() {
                        self.spawn_install(source);
                    } else {
                        self.ghidra.reset();
                    }
                }
                GhidraOutcome::None => {}
            },
            Step::Jdk => {
                // a probe in flight owns the page; escape cancels back to ghidra
                if self.state.jdk_checking {
                    if key.code == KeyCode::Esc {
                        self.cancel_task();
                        self.state.jdk_checking = false;
                        self.goto(Step::Ghidra);
                    }
                    return;
                }
                match key.code {
                    KeyCode::Char('r') => self.spawn_jdk_check(),
                    KeyCode::Enter => self.goto(Step::Projects),
                    KeyCode::Esc => self.goto(Step::Ghidra),
                    _ => {}
                }
            }
            Step::Projects => match self.projects.handle_key(key) {
                ProjectsOutcome::Submit(raw) => self.submit_projects(&raw),
                ProjectsOutcome::Back => {
                    // back lands on the jdk page only when an install was chosen
                    if self.state.plan.ghidra_install.is_some() {
                        self.goto(Step::Jdk);
                    } else {
                        self.goto(Step::Ghidra);
                    }
                }
                ProjectsOutcome::None => {}
            },
            Step::Review => match key.code {
                KeyCode::Enter => self.save(),
                KeyCode::Esc => self.goto(Step::Projects),
                KeyCode::Char('q') => {
                    // quitting without writing takes a deliberate second press
                    if confirm_armed {
                        self.should_quit = true;
                    } else {
                        self.confirm_quit = true;
                    }
                }
                _ => {}
            },
            // the done page exits on any key
            Step::Done => self.should_quit = true,
        }
    }

    /// Move to a step, cancelling whatever task the old step had in flight
    ///
    /// # Arguments
    ///
    /// * `step` - The step to move to
    fn goto(&mut self, step: Step) {
        // a step change invalidates the old step's background work
        self.cancel_task();
        self.step = step;
        // entering the jdk page kicks its probe off immediately
        if step == Step::Jdk {
            self.spawn_jdk_check();
        }
    }

    /// Bump the generation and hand it out for a fresh background task
    fn begin_task(&mut self) -> u64 {
        // cancelling bumps the generation, orphaning any in-flight results
        self.cancel_task();
        self.generation
    }

    /// Cancel the in-flight background task, if any, and clean its leavings
    fn cancel_task(&mut self) {
        // bump the generation so late results from this task always drop
        self.generation += 1;
        // abort the task itself; blocking sections finish but their results drop
        if let Some(task) = self.task.take() {
            task.abort();
        }
        // clear the in-flight flags so pages stop spinning
        self.state.logging_in = false;
        self.state.checking = false;
        self.state.jdk_checking = false;
        // a cancelled download's partial file is useless; drop it
        if let Some(part) = self.part_file.take() {
            let _ = std::fs::remove_file(part);
        }
    }

    /// Spawn the Thorium login task
    ///
    /// # Arguments
    ///
    /// * `url` - The Thorium API url to log into
    /// * `username` - The username to authenticate as
    /// * `password` - The password to authenticate with
    fn spawn_login(&mut self, url: String, username: String, password: String) {
        let generation = self.begin_task();
        self.state.logging_in = true;
        let events = self.event_tx.clone();
        self.task = Some(tokio::spawn(async move {
            // run the login and post however it went; a dropped channel just
            // means the app is gone
            match login(url, username, password).await {
                Ok((keys, username)) => {
                    let _ = events
                        .send(SetupEvent::LoginDone {
                            generation,
                            keys,
                            username,
                        })
                        .await;
                }
                Err(error) => {
                    let _ = events
                        .send(SetupEvent::LoginFailed {
                            generation,
                            error: error.to_string(),
                        })
                        .await;
                }
            }
        }));
    }

    /// Spawn the provider check task over the drafted settings
    fn spawn_check(&mut self) {
        // the check targets the in-wizard draft, never the saved config
        let Some(resolved) = self.state.plan.provider.clone() else {
            return;
        };
        let generation = self.begin_task();
        self.state.checking = true;
        self.state.check = None;
        let events = self.event_tx.clone();
        self.task = Some(tokio::spawn(async move {
            // one bare completion round trip, exactly like `thorctl analyze check`,
            // keeping the provider's full error chain
            let provider = session::to_thorpal_provider(resolved);
            match provider.check(CHECK_PROMPT).await {
                Ok(reply) => {
                    let _ = events
                        .send(SetupEvent::CheckDone {
                            generation,
                            reply: reply.trim().to_string(),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = events
                        .send(SetupEvent::CheckFailed {
                            generation,
                            error: format!("{error:#}"),
                        })
                        .await;
                }
            }
        }));
    }

    /// Spawn the Ghidra install pipeline
    ///
    /// # Arguments
    ///
    /// * `source` - Where the release zip comes from
    fn spawn_install(&mut self, source: InstallSource) {
        let generation = self.begin_task();
        // remember the source for the failed page's retry
        self.install_source = Some(source.clone());
        // the page flips to its progress face immediately
        self.ghidra.mode = GhidraMode::Installing(InstallPhase::Resolving);
        let events = self.event_tx.clone();
        let root = self.state.install_root.clone();
        self.task = Some(tokio::spawn(ghidra::run_install(
            root, source, generation, events,
        )));
    }

    /// Spawn the JDK probe for the chosen install
    fn spawn_jdk_check(&mut self) {
        let install = self.state.plan.ghidra_install.clone();
        let generation = self.begin_task();
        self.state.jdk_checking = true;
        self.state.jdk = None;
        let events = self.event_tx.clone();
        self.task = Some(tokio::spawn(async move {
            // the probe wrapper already keeps the blocking work off the loop
            let report = jdk::check_jdk(install).await;
            let _ = events
                .send(SetupEvent::JdkChecked { generation, report })
                .await;
        }));
    }

    /// Validate and accept the projects directory, then move to review
    ///
    /// # Arguments
    ///
    /// * `raw` - The projects directory as typed
    fn submit_projects(&mut self, raw: &str) {
        // absolutize so the saved path resolves from any later working dir
        let cleaned = match session::clean_absolute(Path::new(raw)) {
            Ok(cleaned) => cleaned,
            Err(error) => {
                self.projects.error = Some(error.to_string());
                return;
            }
        };
        // reject a dir Ghidra itself would refuse (dot-prefixed components)
        if let Err(error) = thorpal::validate_projects_dir(&cleaned) {
            self.projects.error = Some(error);
            return;
        }
        self.projects.error = None;
        self.state.plan.ghidra_projects = Some(cleaned);
        self.goto(Step::Review);
    }

    /// Merge the plan over the loaded config and write it atomically
    fn save(&mut self) {
        // re-load the config at save time so anything written while the
        // wizard sat open (say, a `thorctl login` in another terminal) is
        // merged over rather than clobbered by the startup snapshot; the
        // plan's own answers still win inside apply
        let (fresh, _) = config::load_state(&self.state.config_path);
        let existing = fresh.or_else(|| self.state.existing.clone());
        // the merge validates the provider selection before anything writes
        let merged = match config::apply(&self.state.plan, existing, &self.state.default_projects) {
            Ok(merged) => merged,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        // land the merged config atomically and move to the done page
        match config::write_atomic(&self.state.config_path, &merged) {
            Ok(()) => self.goto(Step::Done),
            Err(error) => self.error = Some(format!("saving failed: {error}")),
        }
    }

    /// Soft-warn in the footer when the install root's disk looks tight
    ///
    /// # Arguments
    ///
    /// * `size` - The release asset's size in bytes
    fn warn_on_low_disk(&mut self, size: u64) {
        // an unknown asset size cannot be reasoned about
        if size == 0 {
            return;
        }
        // the zip plus the extracted tree runs about four times the asset
        let needed_gib = (size.saturating_mul(4)).div_ceil(1024 * 1024 * 1024);
        // the probe is best-effort: statvfs failures never block an install
        if let Ok(free_gib) = disk_free_gib(&self.state.install_root)
            && free_gib < needed_gib
        {
            self.error = Some(format!(
                "low disk space: {free_gib} GiB free at {}, the install needs \
                about {needed_gib} GiB",
                self.state.install_root.display()
            ));
        }
    }

    /// The steps this run's breadcrumb shows, in order
    fn visible_steps(&self) -> Vec<Step> {
        // the login step only exists on fresh boxes
        let mut steps = vec![Step::Welcome];
        if self.login_needed() {
            steps.push(Step::Login);
        }
        steps.extend([
            Step::Provider,
            Step::Credentials,
            Step::Verify,
            Step::Ghidra,
            Step::Jdk,
            Step::Projects,
            Step::Review,
        ]);
        steps
    }

    /// Renders one frame: the breadcrumb bar, the step body, and the footer
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    fn render(&mut self, frame: &mut Frame) {
        // carve the chrome off the body
        let [bar, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.render_bar(frame, bar);
        // inset the body a little so pages never touch the chrome
        let body = body.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        match self.step {
            Step::Welcome => welcome::render(frame, body, &self.state),
            Step::Login => self.login.render(frame, body, self.state.logging_in),
            Step::Provider => self.provider.render(frame, body, &self.state),
            Step::Credentials => {
                if let Some(form) = &mut self.credentials {
                    form.render(frame, body);
                }
            }
            Step::Verify => verify::render(frame, body, &self.state),
            Step::Ghidra => self.ghidra.render(frame, body, &self.state),
            Step::Jdk => java::render(frame, body, &self.state),
            Step::Projects => self.projects.render(frame, body),
            Step::Review => review::render(frame, body, &self.state),
            Step::Done => review::render_done(frame, body, &self.state),
        }
        self.render_footer(frame, footer);
    }

    /// Renders the top breadcrumb bar
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The bar's one-line area
    fn render_bar(&self, frame: &mut Frame, area: Rect) {
        // the three breadcrumb states: behind, active, and ahead of the flow
        let done = Style::default().fg(Color::Green);
        let active = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let pending = Style::default().fg(Color::DarkGray);
        let mut spans = vec![Span::raw(" ")];
        for (index, step) in self.visible_steps().into_iter().enumerate() {
            // style by position in the flow; the done page greens everything
            let style = if step == self.step {
                active
            } else if step.order() < self.step.order() {
                done
            } else {
                pending
            };
            if index > 0 {
                spans.push(Span::styled(" › ", pending));
            }
            spans.push(Span::styled(step.label(), style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Renders the bottom status/help footer
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The footer's one-line area
    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        // the left side carries the showing step's actual key bindings
        let help = match self.step {
            Step::Welcome => " enter begin · q quit",
            Step::Login => " tab/↓ field · enter next/submit · esc back",
            Step::Provider => " j/k move · enter choose · o/l/a/b jump · s skip · esc back",
            Step::Credentials => {
                " tab/↓ field · enter advance · ctrl+s submit · ctrl+r reveal · esc back"
            }
            Step::Verify => " t test · s skip · enter continue · esc back",
            Step::Ghidra => match self.ghidra.mode {
                GhidraMode::List => " j/k move · enter choose · s skip · esc back",
                GhidraMode::Manual => " enter validate · esc list",
                GhidraMode::Installing(_) => " esc cancel",
                GhidraMode::Failed(_) => " r retry · esc list",
            },
            Step::Jdk => " r re-check · enter continue · esc back",
            Step::Projects => " enter continue · esc back",
            Step::Review => " enter save · esc back · q quit without saving",
            Step::Done => " any key exits",
        };
        // the right side carries the most pressing status
        let status = if let Some(error) = &self.error {
            Span::styled(format!("{error} "), Style::default().fg(Color::Red))
        } else if self.confirm_quit {
            Span::styled(
                "press q again to quit without saving ".to_string(),
                Style::default().fg(Color::Yellow),
            )
        } else if self.state.logging_in {
            Span::styled(
                "logging in… ".to_string(),
                Style::default().fg(Color::Yellow),
            )
        } else if self.state.checking {
            Span::styled(
                "testing the provider… ".to_string(),
                Style::default().fg(Color::Yellow),
            )
        } else if self.state.jdk_checking {
            Span::styled(
                "probing for a JDK… ".to_string(),
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::raw(String::new())
        };
        // pad the two sides apart across the footer's width
        let width = area.width as usize;
        let pad = width
            .saturating_sub(help.chars().count())
            .saturating_sub(status.content.chars().count());
        let line = Line::from(vec![
            Span::styled(help, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(pad)),
            status,
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// The provider kind a resolved provider's settings belong to
///
/// # Arguments
///
/// * `resolved` - The resolved provider to name
fn kind_of(resolved: &ResolvedAiProvider) -> AiProviderKind {
    match resolved {
        ResolvedAiProvider::OpenAi(_) => AiProviderKind::OpenAi,
        ResolvedAiProvider::LiteLlm(_) => AiProviderKind::LiteLlm,
        ResolvedAiProvider::Anthropic(_) => AiProviderKind::Anthropic,
        ResolvedAiProvider::Bedrock(_) => AiProviderKind::Bedrock,
    }
}
