//! Verify the configured AI provider with a single hello prompt

use thorium::client::conf::ResolvedAiProvider;
use thorium::{CtlConf, Error};

use super::session;

/// The trivial prompt the check sends to the configured model
const CHECK_PROMPT: &str = "hello";

/// Verify the configured AI provider responds to a trivial prompt
///
/// Sends a single prompt through the provider named by the `ai` block with no
/// tools, sub-agents, or TUI, so provider access (endpoints, API keys, the AWS
/// credential chain for Bedrock) can be tested in isolation. The Thorium API
/// is never contacted. A failure surfaces the provider's full error chain
/// rather than the flattened message the chat TUI shows.
///
/// # Arguments
///
/// * `conf` - The Thorctl config, whose `ai` block names the provider to check
pub async fn handle(conf: &CtlConf) -> Result<(), Error> {
    // ThorPal requires an ai block naming the LLM provider to talk to
    let ai = conf.ai.as_ref().ok_or_else(|| {
        Error::new(
            "No `ai` config set: ThorPal needs an `ai` block with a provider \
            section such as `openai`, `litellm`, `anthropic`, or `bedrock`",
        )
    })?;
    // resolve which provider is active so the check targets what chat would use
    let resolved = ai.resolve()?;
    // describe the target before the conversion below consumes it
    println!(
        "Checking {} with a '{CHECK_PROMPT}' prompt (no tools or sub-agents)...",
        describe(&resolved),
    );
    // run one bare completion round trip, keeping the provider's full error
    // chain so e.g. the underlying AWS error is visible
    let provider = session::to_thorpal_provider(resolved);
    let reply = provider
        .check(CHECK_PROMPT)
        .await
        .map_err(|err| Error::new(format!("{err:#}")))?;
    // show the reply as proof the provider round trip worked
    println!("Reply: {}", reply.trim());
    println!("AI provider check passed");
    Ok(())
}

/// Describe the provider and model a resolved config talks to
///
/// Also used by the setup wizard's test-connection step, so both commands
/// name the target the same way.
///
/// # Arguments
///
/// * `resolved` - The active provider resolved from the Thorctl config
pub(super) fn describe(resolved: &ResolvedAiProvider) -> String {
    match resolved {
        ResolvedAiProvider::OpenAi(openai) => match &openai.endpoint {
            Some(endpoint) => format!("openai model '{}' at {endpoint}", openai.model),
            None => format!("openai model '{}'", openai.model),
        },
        ResolvedAiProvider::LiteLlm(litellm) => {
            format!("litellm model '{}' at {}", litellm.model, litellm.endpoint)
        }
        ResolvedAiProvider::Anthropic(anthropic) => match &anthropic.endpoint {
            Some(endpoint) => format!("anthropic model '{}' at {endpoint}", anthropic.model),
            None => format!("anthropic model '{}'", anthropic.model),
        },
        ResolvedAiProvider::Bedrock(bedrock) => {
            // name the credential source the request will authenticate with
            let credentials = if bedrock.access_key_id.is_some() {
                "explicit credentials".to_owned()
            } else {
                match (&bedrock.region, &bedrock.profile) {
                    (Some(region), _) => format!("region {region}"),
                    (None, Some(profile)) => format!("profile {profile}"),
                    (None, None) => "the environment credential chain".to_owned(),
                }
            };
            // say which of the two Bedrock endpoints the config selects
            let endpoint = if bedrock.mantle {
                "bedrock mantle"
            } else {
                "bedrock"
            };
            format!("{endpoint} model '{}' via {credentials}", bedrock.model)
        }
    }
}
