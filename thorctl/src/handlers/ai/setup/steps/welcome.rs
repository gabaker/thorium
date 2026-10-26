//! The welcome page: what the wizard found on this box and what comes next

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::super::app::WizardState;
use super::{dim_style, spinner};

/// A labeled status line for the welcome summary
///
/// # Arguments
///
/// * `label` - The row's left-hand label
/// * `ok` - Whether the row reads as healthy
/// * `value` - The row's status text
fn status_line(label: &str, ok: bool, value: String) -> Line<'static> {
    // healthy rows get a green check, the rest an amber dot
    let (marker, color) = if ok {
        ("✓ ", Color::Green)
    } else {
        ("• ", Color::Yellow)
    };
    Line::from(vec![
        Span::styled(marker.to_string(), Style::default().fg(color)),
        Span::styled(
            format!("{label:<10}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(value, Style::default().fg(Color::Gray)),
    ])
}

/// Render the welcome page's detected-state summary
///
/// # Arguments
///
/// * `frame` - The frame to render into
/// * `area` - The body area to render in
/// * `state` - The wizard's shared state
pub fn render(frame: &mut Frame, area: Rect, state: &WizardState) {
    let mut lines = vec![
        Line::from(Span::styled(
            "ThorPal setup",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::styled(
            "This wizard configures the AI side of thorctl: an LLM provider and its \
            credentials, a Ghidra install for binary analysis, the full JDK Ghidra \
            needs, and the Ghidra projects directory. Nothing is written until the \
            final review step is confirmed.",
            dim_style(),
        ),
        Line::raw(""),
    ];
    // describe the config file this run reads and will write
    let config = if let Some(error) = &state.config_error {
        status_line(
            "config",
            false,
            format!(
                "'{}' exists but does not parse (saving will overwrite it): {error}",
                state.config_path.display()
            ),
        )
    } else if state.existing.is_some() {
        status_line("config", true, format!("{}", state.config_path.display()))
    } else {
        status_line(
            "config",
            false,
            format!(
                "none at '{}' — the wizard will offer a Thorium login",
                state.config_path.display()
            ),
        )
    };
    lines.push(config);
    // describe the currently configured provider, when one resolves
    let provider = match state.existing.as_ref().and_then(|conf| conf.ai.as_ref()) {
        Some(ai) => match ai.resolve() {
            Ok(resolved) => status_line(
                "provider",
                true,
                crate::handlers::ai::check::describe(&resolved),
            ),
            Err(error) => status_line("provider", false, error.to_string()),
        },
        None => status_line(
            "provider",
            false,
            "no `ai` block configured yet".to_string(),
        ),
    };
    lines.push(provider);
    // describe what the background environment scan found so far
    match &state.detected {
        Some(detected) => {
            // the ghidra line names the first find and counts the rest
            let ghidra = match detected.installs.as_slice() {
                [] => status_line("ghidra", false, "no install found on this box".to_string()),
                [first] => status_line("ghidra", true, format!("{}", first.display())),
                [first, rest @ ..] => status_line(
                    "ghidra",
                    true,
                    format!("{} (+{} more)", first.display(), rest.len()),
                ),
            };
            lines.push(ghidra);
            // the java line carries the probe's own summary
            lines.push(status_line(
                "java",
                detected.jdk.ok,
                detected.jdk.summary.clone(),
            ));
        }
        None => lines.push(Line::styled(
            format!(
                "{} scanning this box for Ghidra installs and a JDK…",
                spinner()
            ),
            dim_style(),
        )),
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Press enter to begin.", dim_style()));
    // render the assembled summary with wrapping for narrow terminals
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
