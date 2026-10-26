//! The review page (what will be written and where) and the done page

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use thorium::client::conf::ResolvedAiProvider;

use super::super::app::WizardState;
use super::{dim_style, mask_secret};

/// A labeled review row with a change marker
///
/// # Arguments
///
/// * `label` - The row's left-hand label
/// * `value` - The row's value text
/// * `marker` - The change marker shown after the value, if any
fn row(label: &str, value: String, marker: Option<(&str, Color)>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("  {label:<12}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(value, Style::default().fg(Color::Gray)),
    ];
    if let Some((text, color)) = marker {
        spans.push(Span::styled(
            format!("  ({text})"),
            Style::default().fg(color),
        ));
    }
    Line::from(spans)
}

/// The masked credential summary a drafted provider writes
///
/// # Arguments
///
/// * `provider` - The drafted provider settings
fn credential_summary(provider: &ResolvedAiProvider) -> String {
    match provider {
        ResolvedAiProvider::OpenAi(settings) => mask_secret(&settings.api_key),
        ResolvedAiProvider::Anthropic(settings) => mask_secret(&settings.api_key),
        ResolvedAiProvider::LiteLlm(settings) => match &settings.api_key {
            Some(key) => mask_secret(key),
            None => "no auth (open gateway)".to_string(),
        },
        ResolvedAiProvider::Bedrock(settings) => match &settings.access_key_id {
            Some(key) => format!("explicit AWS key {}", mask_secret(key)),
            None => "the AWS credential chain".to_string(),
        },
    }
}

/// Render the review page
///
/// # Arguments
///
/// * `frame` - The frame to render into
/// * `area` - The body area to render in
/// * `state` - The wizard's shared state
pub fn render(frame: &mut Frame, area: Rect, state: &WizardState) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Review",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];
    // the thorium row says where the keys block comes from
    let existing_ai = state.existing.as_ref().and_then(|conf| conf.ai.as_ref());
    let thorium = if let Some(username) = &state.logged_in_as {
        row(
            "thorium",
            format!("logged in as {username}"),
            Some(("new token", Color::Yellow)),
        )
    } else if state.login_skipped {
        row(
            "thorium",
            "login skipped — placeholder keys, run `thorctl login` later".to_string(),
            Some(("placeholder", Color::Yellow)),
        )
    } else {
        row(
            "thorium",
            "kept from the existing config".to_string(),
            Some(("kept", Color::Green)),
        )
    };
    lines.push(thorium);
    // the provider rows describe the drafted or kept selection
    match &state.plan.provider {
        Some(provider) => {
            lines.push(row(
                "provider",
                crate::handlers::ai::check::describe(provider),
                Some(("changed", Color::Yellow)),
            ));
            lines.push(row("credential", credential_summary(provider), None));
        }
        None => match existing_ai.map(|ai| ai.resolve()) {
            Some(Ok(resolved)) => lines.push(row(
                "provider",
                crate::handlers::ai::check::describe(&resolved),
                Some(("kept", Color::Green)),
            )),
            _ => lines.push(row(
                "provider",
                "none configured".to_string(),
                Some(("unset", Color::Yellow)),
            )),
        },
    }
    // the ghidra install row
    match &state.plan.ghidra_install {
        Some(install) => {
            // say whether this differs from what the config already had
            let marker = if existing_ai.and_then(|ai| ai.ghidra_install.as_ref()) == Some(install) {
                ("kept", Color::Green)
            } else {
                ("changed", Color::Yellow)
            };
            lines.push(row(
                "ghidra",
                format!("{}", install.display()),
                Some(marker),
            ));
        }
        None => match existing_ai.and_then(|ai| ai.ghidra_install.as_ref()) {
            Some(install) => lines.push(row(
                "ghidra",
                format!("{}", install.display()),
                Some(("kept", Color::Green)),
            )),
            None => lines.push(row(
                "ghidra",
                "not configured (skipped)".to_string(),
                Some(("unset", Color::Yellow)),
            )),
        },
    }
    // the projects dir row, naming the implicit default when it applies
    match &state.plan.ghidra_projects {
        Some(projects)
            if projects == &state.default_projects
                && existing_ai
                    .and_then(|ai| ai.ghidra_projects.as_ref())
                    .is_none() =>
        {
            lines.push(row(
                "projects",
                format!("{} ", projects.display()),
                Some(("default, not pinned", Color::Green)),
            ));
        }
        Some(projects) => lines.push(row(
            "projects",
            format!("{}", projects.display()),
            Some(("pinned", Color::Yellow)),
        )),
        None => lines.push(row(
            "projects",
            format!("{} ", state.default_projects.display()),
            Some(("default", Color::Green)),
        )),
    }
    // the write target, warning loudly when it overwrites a broken file
    lines.push(Line::raw(""));
    lines.push(row(
        "writes to",
        format!("{}", state.config_path.display()),
        None,
    ));
    if state.config_error.is_some() {
        lines.push(Line::styled(
            "  ⚠ the existing file did not parse and WILL be overwritten",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Press enter to save, esc to go back, or q twice to quit without writing.",
        dim_style(),
    ));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Render the done page
///
/// # Arguments
///
/// * `frame` - The frame to render into
/// * `area` - The body area to render in
/// * `state` - The wizard's shared state
pub fn render_done(frame: &mut Frame, area: Rect, state: &WizardState) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("✓ ", Style::default().fg(Color::Green)),
            Span::styled(
                format!("wrote {}", state.config_path.display()),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::styled("Next steps:", dim_style()),
        Line::styled(
            "  thorctl analyze check          verify the provider round trip any time",
            dim_style(),
        ),
        Line::styled(
            "  thorctl analyze chat <binary>  start analyzing a binary with ThorPal",
            dim_style(),
        ),
    ];
    // remind about anything the wizard left for later
    if state.login_skipped {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "⚠ the Thorium login was skipped: run `thorctl login` before any \
            command that talks to Thorium itself",
            Style::default().fg(Color::Yellow),
        ));
    }
    if state.jdk.as_ref().is_some_and(|report| !report.ok) {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "⚠ no usable JDK was found: install one before running Ghidra analysis",
            Style::default().fg(Color::Yellow),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Press any key to exit.", dim_style()));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
