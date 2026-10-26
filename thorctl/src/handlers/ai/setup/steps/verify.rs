//! The test-connection page: one optional paid round trip through the provider

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::super::app::WizardState;
use super::{dim_style, spinner};

/// Render the test-connection page
///
/// # Arguments
///
/// * `frame` - The frame to render into
/// * `area` - The body area to render in
/// * `state` - The wizard's shared state
pub fn render(frame: &mut Frame, area: Rect, state: &WizardState) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Test the provider",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];
    // name the exact target the check would talk to
    if let Some(resolved) = &state.plan.provider {
        lines.push(Line::styled(
            format!("Target: {}", crate::handlers::ai::check::describe(resolved)),
            Style::default().fg(Color::Gray),
        ));
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        "The test sends one tiny 'hello' completion (a real, billed request) with \
        no tools or sub-agents, so endpoint and credential problems surface here \
        instead of mid-analysis. It is optional.",
        dim_style(),
    ));
    lines.push(Line::raw(""));
    // show the live/finished state of the check
    if state.checking {
        lines.push(Line::styled(
            format!("{} testing the provider…", spinner()),
            Style::default().fg(Color::Yellow),
        ));
    } else {
        match &state.check {
            Some(Ok(reply)) => {
                lines.push(Line::from(vec![
                    Span::styled("✓ check passed", Style::default().fg(Color::Green)),
                    Span::styled(
                        format!(" — reply: {reply}"),
                        Style::default().fg(Color::Gray),
                    ),
                ]));
                lines.push(Line::raw(""));
                lines.push(Line::styled("Press enter to continue.", dim_style()));
            }
            Some(Err(error)) => {
                lines.push(Line::styled(
                    "✗ check failed",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Press esc to go back and fix the credentials, t to retry, or s \
                    to continue anyway.",
                    dim_style(),
                ));
            }
            None => lines.push(Line::styled(
                "Press t to run the test, or s to skip it.",
                dim_style(),
            )),
        }
    }
    // render the assembled page with wrapping for long error chains
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
