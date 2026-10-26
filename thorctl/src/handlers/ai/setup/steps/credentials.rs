//! The per-provider credentials form: endpoints, keys, and the model

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use thorium::client::conf::{
    AISettings, AiProviderKind, AnthropicSettings, BedrockSettings, LiteLlmSettings,
    OpenAiSettings, ResolvedAiProvider,
};

use crate::handlers::ai::chat::TextBox;

use super::{dim_style, pointer, selected_style};

/// Which provider setting a form field edits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKey {
    /// The provider's base url
    Endpoint,
    /// The provider's API key
    ApiKey,
    /// The model to talk to
    Model,
    /// The AWS region to talk to Bedrock in
    Region,
    /// The AWS profile to resolve credentials from
    Profile,
    /// An explicit AWS access key id
    AccessKeyId,
    /// The secret access key paired with the access key id
    SecretAccessKey,
    /// The session token accompanying the explicit key pair
    SessionToken,
}

impl FieldKey {
    /// The human-facing label errors and titles name this field with
    fn label(self) -> &'static str {
        match self {
            FieldKey::Endpoint => "endpoint",
            FieldKey::ApiKey => "api key",
            FieldKey::Model => "model",
            FieldKey::Region => "region",
            FieldKey::Profile => "profile",
            FieldKey::AccessKeyId => "access key id",
            FieldKey::SecretAccessKey => "secret access key",
            FieldKey::SessionToken => "session token",
        }
    }
}

/// One field of the credentials form
enum FormField {
    /// A text field backed by a text box
    Text {
        /// The provider setting this field edits
        key: FieldKey,
        /// Whether the provider requires this field
        required: bool,
        /// Whether the field holds a secret and renders masked
        secret: bool,
        /// The text box holding the field's value
        boxed: TextBox,
    },
    /// The Bedrock Mantle on/off toggle
    Toggle {
        /// The toggle's current value
        value: bool,
    },
}

/// What a key press on the credentials form asks the app to do
pub enum CredsOutcome {
    /// Nothing; the form consumed the key itself
    None,
    /// The form validated and built the drafted provider settings
    Submit(Box<ResolvedAiProvider>),
    /// Go back one step
    Back,
}

/// The credentials form's state for one provider
pub struct CredentialsStep {
    /// The provider this form drafts settings for
    kind: AiProviderKind,
    /// The form's fields in render/focus order
    fields: Vec<FormField>,
    /// Which field holds focus
    focus: usize,
    /// Whether secret fields render revealed instead of masked
    revealed: bool,
    /// The last validation error, shown inline until the next attempt
    pub error: Option<String>,
}

impl CredentialsStep {
    /// Create the form for a provider, prefilled from the loaded config
    ///
    /// # Arguments
    ///
    /// * `kind` - The provider the form drafts settings for
    /// * `ai` - The loaded ai block the fields prefill from, if any
    pub fn new(kind: AiProviderKind, ai: Option<&AISettings>) -> Self {
        // a text field prefilled from the existing block when one exists
        let text = |key: FieldKey, required: bool, secret: bool, value: Option<&str>| {
            // name the field and whether it may be left empty in the title
            let title = if required {
                format!("{} (required)", key.label())
            } else {
                format!("{} (optional)", key.label())
            };
            let mut boxed = TextBox::default().title(title);
            if secret {
                boxed = boxed.masked();
            }
            if let Some(value) = value {
                boxed.set_content(value.to_string());
            }
            FormField::Text {
                key,
                required,
                secret,
                boxed,
            }
        };
        // build the provider's field list with its block's current values
        let fields = match kind {
            AiProviderKind::OpenAi => {
                let block = ai.and_then(|ai| ai.openai.as_ref());
                vec![
                    text(
                        FieldKey::Endpoint,
                        false,
                        false,
                        block.and_then(|b| b.endpoint.as_deref()),
                    ),
                    text(
                        FieldKey::ApiKey,
                        true,
                        true,
                        block.map(|b| b.api_key.as_str()),
                    ),
                    text(
                        FieldKey::Model,
                        true,
                        false,
                        block.map(|b| b.model.as_str()),
                    ),
                ]
            }
            AiProviderKind::LiteLlm => {
                let block = ai.and_then(|ai| ai.litellm.as_ref());
                // a legacy flat config prefills the litellm form too
                let legacy_endpoint = ai.and_then(|ai| ai.endpoint.as_deref());
                let legacy_key = ai.and_then(|ai| ai.api_key.as_deref());
                let legacy_model = ai.and_then(|ai| ai.model.as_deref());
                vec![
                    text(
                        FieldKey::Endpoint,
                        true,
                        false,
                        block.map(|b| b.endpoint.as_str()).or(legacy_endpoint),
                    ),
                    text(
                        FieldKey::ApiKey,
                        false,
                        true,
                        block.and_then(|b| b.api_key.as_deref()).or(legacy_key),
                    ),
                    text(
                        FieldKey::Model,
                        true,
                        false,
                        block.map(|b| b.model.as_str()).or(legacy_model),
                    ),
                ]
            }
            AiProviderKind::Anthropic => {
                let block = ai.and_then(|ai| ai.anthropic.as_ref());
                vec![
                    text(
                        FieldKey::Endpoint,
                        false,
                        false,
                        block.and_then(|b| b.endpoint.as_deref()),
                    ),
                    text(
                        FieldKey::ApiKey,
                        true,
                        true,
                        block.map(|b| b.api_key.as_str()),
                    ),
                    text(
                        FieldKey::Model,
                        true,
                        false,
                        block.map(|b| b.model.as_str()),
                    ),
                ]
            }
            AiProviderKind::Bedrock => {
                let block = ai.and_then(|ai| ai.bedrock.as_ref());
                vec![
                    text(
                        FieldKey::Model,
                        true,
                        false,
                        block.map(|b| b.model.as_str()),
                    ),
                    text(
                        FieldKey::Region,
                        false,
                        false,
                        block.and_then(|b| b.region.as_deref()),
                    ),
                    text(
                        FieldKey::Profile,
                        false,
                        false,
                        block.and_then(|b| b.profile.as_deref()),
                    ),
                    FormField::Toggle {
                        value: block.is_some_and(|b| b.mantle),
                    },
                    text(
                        FieldKey::AccessKeyId,
                        false,
                        false,
                        block.and_then(|b| b.access_key_id.as_deref()),
                    ),
                    text(
                        FieldKey::SecretAccessKey,
                        false,
                        true,
                        block.and_then(|b| b.secret_access_key.as_deref()),
                    ),
                    text(
                        FieldKey::SessionToken,
                        false,
                        true,
                        block.and_then(|b| b.session_token.as_deref()),
                    ),
                ]
            }
        };
        CredentialsStep {
            kind,
            fields,
            focus: 0,
            revealed: false,
            error: None,
        }
    }

    /// Handle a key press on the credentials form
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> CredsOutcome {
        // escape always backs out of the form
        if key.code == KeyCode::Esc {
            return CredsOutcome::Back;
        }
        // the control chords: reveal secrets, submit the whole form
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('r') => self.revealed = !self.revealed,
                KeyCode::Char('s') => return self.submit(),
                // other control chords never type into a credential field
                _ => {}
            }
            return CredsOutcome::None;
        }
        // tab/arrows walk the fields in order with wraparound
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.focus = (self.focus + 1) % self.fields.len();
                return CredsOutcome::None;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
                return CredsOutcome::None;
            }
            _ => {}
        }
        // route the key into the focused field
        let last = self.focus == self.fields.len() - 1;
        match &mut self.fields[self.focus] {
            FormField::Toggle { value } => match key.code {
                // space flips the toggle, enter moves on
                KeyCode::Char(' ') => *value = !*value,
                KeyCode::Enter => self.focus = (self.focus + 1) % self.fields.len(),
                _ => {}
            },
            FormField::Text {
                required, boxed, ..
            } => {
                // enter on a satisfied field advances; on the last it submits
                let submitted = boxed.handle_key(key, !*required);
                if submitted {
                    if last {
                        return self.submit();
                    }
                    self.focus += 1;
                }
            }
        }
        CredsOutcome::None
    }

    /// Insert pasted text into the focused field
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub fn paste(&mut self, text: &str) {
        // pastes land in text fields only; keys arrive with trailing newlines
        // from some terminals, which a credential never legitimately contains
        if let FormField::Text { boxed, .. } = &mut self.fields[self.focus] {
            boxed.insert_str(text.trim_end_matches(['\r', '\n']));
        }
    }

    /// The trimmed content of a field, `None` when left empty
    ///
    /// # Arguments
    ///
    /// * `key` - The field to read
    fn text(&self, key: FieldKey) -> Option<String> {
        // find the field by key and hand back its non-empty trimmed content
        self.fields.iter().find_map(|field| match field {
            FormField::Text {
                key: field_key,
                boxed,
                ..
            } if *field_key == key => {
                let trimmed = boxed.content.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            _ => None,
        })
    }

    /// A required field's content, or the error naming what is missing
    ///
    /// # Arguments
    ///
    /// * `key` - The required field to read
    fn required(&self, key: FieldKey) -> Result<String, String> {
        self.text(key)
            .ok_or_else(|| format!("the {} is required", key.label()))
    }

    /// Validate the form and build the drafted provider settings
    fn submit(&mut self) -> CredsOutcome {
        // build the provider block from the fields, surfacing the first miss
        let built = self.build();
        match built {
            Ok(resolved) => {
                self.error = None;
                CredsOutcome::Submit(Box::new(resolved))
            }
            Err(error) => {
                // park focus on the first offending field for a quick fix
                if let Some(position) = self.first_missing_required() {
                    self.focus = position;
                }
                self.error = Some(error);
                CredsOutcome::None
            }
        }
    }

    /// The position of the first required text field left empty, if any
    fn first_missing_required(&self) -> Option<usize> {
        self.fields.iter().position(|field| match field {
            FormField::Text {
                required, boxed, ..
            } => *required && boxed.content.trim().is_empty(),
            FormField::Toggle { .. } => false,
        })
    }

    /// Build the provider settings this form drafts
    fn build(&self) -> Result<ResolvedAiProvider, String> {
        // every form leaves pricing untouched; the merge preserves old values
        match self.kind {
            AiProviderKind::OpenAi => Ok(ResolvedAiProvider::OpenAi(OpenAiSettings {
                endpoint: self.text(FieldKey::Endpoint),
                api_key: self.required(FieldKey::ApiKey)?,
                model: self.required(FieldKey::Model)?,
                pricing: None,
            })),
            AiProviderKind::LiteLlm => Ok(ResolvedAiProvider::LiteLlm(LiteLlmSettings {
                endpoint: self.required(FieldKey::Endpoint)?,
                api_key: self.text(FieldKey::ApiKey),
                model: self.required(FieldKey::Model)?,
                pricing: None,
            })),
            AiProviderKind::Anthropic => Ok(ResolvedAiProvider::Anthropic(AnthropicSettings {
                endpoint: self.text(FieldKey::Endpoint),
                api_key: self.required(FieldKey::ApiKey)?,
                model: self.required(FieldKey::Model)?,
                pricing: None,
            })),
            AiProviderKind::Bedrock => {
                // an explicit key pair only makes sense complete
                let access_key_id = self.text(FieldKey::AccessKeyId);
                let secret_access_key = self.text(FieldKey::SecretAccessKey);
                if access_key_id.is_some() != secret_access_key.is_some() {
                    return Err("an explicit AWS key needs both the access key id and the \
                        secret access key (or neither, for the credential chain)"
                        .to_string());
                }
                Ok(ResolvedAiProvider::Bedrock(BedrockSettings {
                    region: self.text(FieldKey::Region),
                    profile: self.text(FieldKey::Profile),
                    mantle: self.mantle(),
                    access_key_id,
                    secret_access_key,
                    session_token: self.text(FieldKey::SessionToken),
                    model: self.required(FieldKey::Model)?,
                    pricing: None,
                }))
            }
        }
    }

    /// The Mantle toggle's current value
    fn mantle(&self) -> bool {
        self.fields.iter().any(|field| match field {
            FormField::Toggle { value } => *value,
            FormField::Text { .. } => false,
        })
    }

    /// Render the credentials form
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // carve a row set: an intro, each field, and a trailing status strip
        let mut constraints = vec![Constraint::Length(2)];
        for field in &self.fields {
            constraints.push(match field {
                FormField::Text { .. } => Constraint::Length(3),
                FormField::Toggle { .. } => Constraint::Length(1),
            });
        }
        constraints.push(Constraint::Min(0));
        let rows = Layout::vertical(constraints).split(area);
        // the intro names the provider and the form's chords
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(
                    "{} credentials — tab/↓ next field, enter advances (and submits \
                    on the last), ctrl+s submits, ctrl+r reveals secrets",
                    self.kind.name()
                ),
                dim_style(),
            ))
            .wrap(Wrap { trim: false }),
            rows[0],
        );
        // render each field into its row, focusing exactly one
        for (index, field) in self.fields.iter_mut().enumerate() {
            let focused = index == self.focus;
            let row = rows[index + 1];
            match field {
                FormField::Text { secret, boxed, .. } => {
                    // reveal flips masking off for every secret field at once
                    boxed.masked = *secret && !self.revealed;
                    boxed.is_focused = focused;
                    boxed.render(frame, row);
                }
                FormField::Toggle { value } => {
                    // the toggle renders as a focusable checkbox row
                    let marker = if *value { "[x]" } else { "[ ]" };
                    let style = if focused {
                        selected_style()
                    } else {
                        dim_style()
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            pointer(focused),
                            Span::styled(
                                format!(
                                    "{marker} use the Bedrock Mantle (Anthropic Messages \
                                    API) endpoint — space toggles"
                                ),
                                style,
                            ),
                        ])),
                        row,
                    );
                }
            }
        }
        // the status strip carries the last validation error
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(Line::styled(error.clone(), Style::default().fg(Color::Red)))
                    .wrap(Wrap { trim: false }),
                rows[rows.len() - 1],
            );
        }
    }
}
