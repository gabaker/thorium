//! The JDK page: whether the full JDK Ghidra needs is on this box

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::super::app::WizardState;
use super::{dim_style, spinner};

/// Render the JDK check page
///
/// # Arguments
///
/// * `frame` - The frame to render into
/// * `area` - The body area to render in
/// * `state` - The wizard's shared state
pub fn render(frame: &mut Frame, area: Rect, state: &WizardState) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Java check",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::styled(
            "Ghidra compiles scripts at runtime, so it needs a full JDK (javac plus \
            the jdk.compiler module), not just a JRE. The required version comes \
            from the chosen install's application.properties.",
            dim_style(),
        ),
        Line::raw(""),
    ];
    // name the install the probe checked against
    if let Some(install) = &state.plan.ghidra_install {
        lines.push(Line::styled(
            format!("Ghidra install: {}", install.display()),
            Style::default().fg(Color::Gray),
        ));
        lines.push(Line::raw(""));
    }
    // show the live/finished probe state
    if state.jdk_checking {
        lines.push(Line::styled(
            format!("{} probing for a JDK…", spinner()),
            Style::default().fg(Color::Yellow),
        ));
    } else {
        match &state.jdk {
            Some(report) if report.ok => {
                lines.push(Line::from(vec![
                    Span::styled("✓ ", Style::default().fg(Color::Green)),
                    Span::styled(report.summary.clone(), Style::default().fg(Color::Green)),
                ]));
                lines.push(Line::raw(""));
                lines.push(Line::styled("Press enter to continue.", dim_style()));
            }
            Some(report) => {
                lines.push(Line::styled(
                    "✗ no usable JDK found",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
                // the probe's text carries the per-OS install commands verbatim
                for part in report.summary.lines() {
                    lines.push(Line::styled(
                        part.to_string(),
                        Style::default().fg(Color::Red),
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Install a JDK and press r to re-check, or press enter to \
                    continue anyway (Ghidra analysis will fail until one exists).",
                    dim_style(),
                ));
            }
            None => lines.push(Line::styled("Press r to probe for a JDK.", dim_style())),
        }
    }
    // render the assembled page with wrapping for the fix-it text
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
