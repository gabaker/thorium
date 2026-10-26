//! The provider selection page: which LLM provider ThorPal talks to

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use thorium::client::conf::{AISettings, AiProviderKind};

use super::super::app::WizardState;
use super::{dim_style, idle_style, pointer, selected_style};

/// The selectable providers with their accelerator keys and descriptions
const PROVIDERS: [(AiProviderKind, char, &str); 4] = [
    (
        AiProviderKind::OpenAi,
        'o',
        "the OpenAI API, or any endpoint speaking its protocol",
    ),
    (
        AiProviderKind::LiteLlm,
        'l',
        "a LiteLLM (OpenAI-compatible) gateway",
    ),
    (AiProviderKind::Anthropic, 'a', "the Anthropic API"),
    (
        AiProviderKind::Bedrock,
        'b',
        "AWS Bedrock, via the AWS credential chain",
    ),
];

/// The row index of the skip option under the provider rows
const SKIP_ROW: usize = PROVIDERS.len();

/// What a key press on the provider page asks the app to do
pub enum ProviderOutcome {
    /// Nothing; the page consumed the key itself
    None,
    /// Open the credentials form for this provider
    Chosen(AiProviderKind),
    /// Keep the AI provider config exactly as it is
    Skip,
    /// Go back one step
    Back,
}

/// The provider page's selection state
pub struct ProviderStep {
    /// The selected row (the four providers, then skip)
    selected: usize,
}

impl ProviderStep {
    /// Create the provider page, preselecting the active provider
    ///
    /// # Arguments
    ///
    /// * `active` - The provider the loaded config resolves to, if any
    pub fn new(active: Option<AiProviderKind>) -> Self {
        // land the selection on the active provider so enter keeps it
        let selected = active
            .and_then(|kind| PROVIDERS.iter().position(|(row, _, _)| *row == kind))
            .unwrap_or(0);
        ProviderStep { selected }
    }

    /// Handle a key press on the provider page
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> ProviderOutcome {
        match key.code {
            // escape backs out of the page
            KeyCode::Esc => ProviderOutcome::Back,
            // vim keys and arrows walk the rows with wraparound
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % (SKIP_ROW + 1);
                ProviderOutcome::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = (self.selected + SKIP_ROW) % (SKIP_ROW + 1);
                ProviderOutcome::None
            }
            // enter picks the selected row
            KeyCode::Enter => {
                if self.selected == SKIP_ROW {
                    ProviderOutcome::Skip
                } else {
                    ProviderOutcome::Chosen(PROVIDERS[self.selected].0)
                }
            }
            // s skips without walking to the skip row
            KeyCode::Char('s') => ProviderOutcome::Skip,
            // the accelerator letters jump straight to their provider
            KeyCode::Char(pressed) => {
                match PROVIDERS
                    .iter()
                    .find(|(_, accelerator, _)| *accelerator == pressed)
                {
                    Some((kind, _, _)) => ProviderOutcome::Chosen(*kind),
                    None => ProviderOutcome::None,
                }
            }
            _ => ProviderOutcome::None,
        }
    }

    /// Render the provider page
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    /// * `state` - The wizard's shared state
    pub fn render(&self, frame: &mut Frame, area: Rect, state: &WizardState) {
        let mut lines = vec![
            Line::styled(
                "Choose the LLM provider ThorPal talks to. Providers already \
                configured in your config are marked; picking one pre-fills its form.",
                dim_style(),
            ),
            Line::raw(""),
        ];
        // the existing ai block drives the (configured) tags
        let ai = state.existing.as_ref().and_then(|conf| conf.ai.as_ref());
        for (index, (kind, accelerator, description)) in PROVIDERS.iter().enumerate() {
            let selected = index == self.selected;
            let style = if selected {
                selected_style()
            } else {
                idle_style()
            };
            let mut spans = vec![
                pointer(selected),
                Span::styled(format!("{accelerator}  "), dim_style()),
                Span::styled(format!("{:<10}", kind.name()), style),
                Span::styled((*description).to_string(), dim_style()),
            ];
            // tag the blocks that already exist in the loaded config
            if ai.is_some_and(|ai| has_block(ai, *kind)) {
                spans.push(Span::styled(
                    "  (configured)",
                    Style::default().fg(Color::Green),
                ));
            }
            lines.push(Line::from(spans));
        }
        // the skip row keeps whatever the config already says
        let selected = self.selected == SKIP_ROW;
        lines.push(Line::from(vec![
            pointer(selected),
            Span::styled("s  ", dim_style()),
            Span::styled(
                format!("{:<10}", "skip"),
                if selected {
                    selected_style()
                } else {
                    idle_style()
                },
            ),
            Span::styled(
                "keep the AI provider config exactly as it is".to_string(),
                dim_style(),
            ),
        ]));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

/// Whether an ai block already carries settings for a provider
///
/// # Arguments
///
/// * `ai` - The loaded ai block
/// * `kind` - The provider to look for
fn has_block(ai: &AISettings, kind: AiProviderKind) -> bool {
    match kind {
        AiProviderKind::OpenAi => ai.openai.is_some(),
        // the legacy flat keys load as litellm, so they count as that block
        AiProviderKind::LiteLlm => ai.litellm.is_some() || ai.endpoint.is_some(),
        AiProviderKind::Anthropic => ai.anthropic.is_some(),
        AiProviderKind::Bedrock => ai.bedrock.is_some(),
    }
}
