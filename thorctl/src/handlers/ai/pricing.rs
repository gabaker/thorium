//! Model pricing tables and cost estimation for the AI chat TUI.
//!
//! Cost is presentation-side only: ThorPal persists raw token counts and this
//! module turns them into estimated dollars at render time, so a pricing fix
//! retroactively corrects what a resumed session displays. Prices come from a
//! built-in table of well-known models, overridable (or supplied outright for
//! unknown models like LiteLLM aliases) via the `pricing` block on the active
//! provider's Thorctl config.

use thorium::client::conf::AiPricing;
use thorpal::AiProviderConfig;

/// The per-token prices for one model, in dollars per million tokens
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// $/MTok for fresh (uncached) input tokens
    pub input: f64,
    /// $/MTok for input tokens read from the provider's prompt cache
    pub cache_read: f64,
    /// $/MTok for input tokens written into the provider's prompt cache
    pub cache_write: f64,
    /// $/MTok for output tokens
    pub output: f64,
}

/// A shorthand constructor for the built-in pricing table below
///
/// # Arguments
///
/// * `input` - $/MTok for fresh input tokens
/// * `cache_read` - $/MTok for cache-read input tokens
/// * `cache_write` - $/MTok for cache-write input tokens
/// * `output` - $/MTok for output tokens
const fn price(input: f64, cache_read: f64, cache_write: f64, output: f64) -> ModelPricing {
    ModelPricing {
        input,
        cache_read,
        cache_write,
        output,
    }
}

/// The built-in price table for well-known models, in dollars per million tokens
///
/// Matched against the configured model id with [`boundary_match`], so bare
/// (`claude-opus-4-6`), dated (`claude-opus-4-6-20260205`), and Bedrock-prefixed
/// (`us.anthropic.claude-opus-4-6-...-v1:0`) ids all resolve. Ordering is
/// load-bearing: more-specific patterns must come before their prefixes
/// (`claude-sonnet-4-5` before `claude-sonnet-4`, `gpt-4.1-mini` before
/// `gpt-4.1`, `gpt-5.1`/`gpt-5-mini` before `gpt-5`) because `.`/`-` count as
/// boundaries, so boundary matching alone does not disambiguate them.
///
/// Anthropic cache rates are 0.1x input for reads (except where the provider
/// prices reads lower) and 1.25x input for writes -- the 5-minute-TTL estimate,
/// since rig only reports aggregate cache-creation counts. OpenAI's automatic
/// prompt caching charges nothing for writes, so those rows carry a zero write
/// rate. Cross-checked against Anthropic's docs and LiteLLM's community price
/// database (October 2026); the config `pricing` block overrides any drift.
const BUILTIN_PRICING: &[(&str, ModelPricing)] = &[
    // Bedrock prices the mythos preview far above the first-party API, so the
    // prefixed id gets its own more-specific entry ahead of the bare one
    (
        "anthropic.claude-mythos-preview",
        price(27.5, 2.75, 34.375, 137.5),
    ),
    ("claude-mythos-preview", price(10.0, 1.0, 12.5, 50.0)),
    // the current claude generations, most-specific ids first
    ("claude-sonnet-5-5", price(2.0, 0.10, 2.5, 10.0)),
    ("claude-sonnet-4-6", price(3.0, 0.30, 3.75, 15.0)),
    ("claude-sonnet-4-5", price(3.0, 0.30, 3.75, 15.0)),
    ("claude-sonnet-3-7", price(3.0, 0.30, 3.75, 15.0)),
    ("claude-fable-5-1", price(10.0, 0.25, 12.5, 50.0)),
    ("claude-mythos-5-1", price(10.0, 0.25, 12.5, 50.0)),
    ("claude-haiku-5-5", price(0.10, 0.01, 0.125, 0.50)),
    ("claude-haiku-4-5", price(1.0, 0.10, 1.25, 5.0)),
    ("claude-haiku-3-5", price(0.80, 0.08, 1.0, 4.0)),
    ("claude-opus-5-5", price(4.0, 0.20, 5.0, 20.0)),
    ("claude-opus-4-8", price(5.0, 0.50, 6.25, 25.0)),
    ("claude-opus-4-7", price(5.0, 0.50, 6.25, 25.0)),
    ("claude-opus-4-6", price(5.0, 0.50, 6.25, 25.0)),
    ("claude-opus-4-5", price(5.0, 0.50, 6.25, 25.0)),
    ("claude-opus-4-1", price(15.0, 1.5, 18.75, 75.0)),
    // the older claude generations the dated ids above must not fall into
    ("claude-sonnet-5", price(2.0, 0.20, 2.5, 10.0)),
    ("claude-sonnet-4", price(3.0, 0.30, 3.75, 15.0)),
    ("claude-fable-5", price(10.0, 1.0, 12.5, 50.0)),
    ("claude-mythos-5", price(10.0, 1.0, 12.5, 50.0)),
    ("claude-haiku-3", price(0.25, 0.025, 0.3125, 1.25)),
    ("claude-opus-5", price(5.0, 0.50, 6.25, 25.0)),
    ("claude-opus-4", price(15.0, 1.5, 18.75, 75.0)),
    // openai models, most-specific ids first; cache writes are free there
    ("gpt-4.1-mini", price(0.40, 0.10, 0.0, 1.60)),
    ("gpt-4.1-nano", price(0.10, 0.025, 0.0, 0.40)),
    ("gpt-5-mini", price(0.25, 0.025, 0.0, 2.0)),
    ("gpt-5-nano", price(0.05, 0.005, 0.0, 0.40)),
    ("gpt-4o-mini", price(0.15, 0.075, 0.0, 0.60)),
    ("o4-mini", price(1.10, 0.275, 0.0, 4.40)),
    ("gpt-5.2", price(1.75, 0.175, 0.0, 14.0)),
    ("gpt-5.1", price(1.25, 0.125, 0.0, 10.0)),
    ("gpt-4.1", price(2.0, 0.50, 0.0, 8.0)),
    ("gpt-4o", price(2.5, 1.25, 0.0, 10.0)),
    ("gpt-5", price(1.25, 0.125, 0.0, 10.0)),
    ("o3", price(2.0, 0.50, 0.0, 8.0)),
];

/// Checks whether `pattern` occurs in `model` on token boundaries
///
/// A match's neighboring characters must be absent or non-alphanumeric, so
/// `us.anthropic.claude-opus-4-6-20260205-v1:0` matches `claude-opus-4-6`
/// while `pro3` does not match `o3`. Both sides are compared case-insensitively.
///
/// # Arguments
///
/// * `model` - The lowercased configured model id to search
/// * `pattern` - The lowercase table pattern to look for
fn boundary_match(model: &str, pattern: &str) -> bool {
    // scan every occurrence of the pattern, keeping byte offsets to inspect edges
    let mut start = 0;
    while let Some(found) = model[start..].find(pattern) {
        // translate the relative hit back into an absolute byte offset
        let begin = start + found;
        let end = begin + pattern.len();
        // the character before the hit must be absent or a separator
        let left_ok = begin == 0
            || model[..begin]
                .chars()
                .next_back()
                .is_some_and(|ch| !ch.is_ascii_alphanumeric());
        // the character after the hit must be absent or a separator
        let right_ok = end == model.len()
            || model[end..]
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_ascii_alphanumeric());
        // a hit with clean edges on both sides is a boundary match
        if left_ok && right_ok {
            return true;
        }
        // otherwise keep scanning past this embedded hit
        start = begin + 1;
    }
    false
}

/// Looks a model id up in the built-in price table
///
/// # Arguments
///
/// * `model` - The configured model id (bare, dated, or provider-prefixed)
pub fn builtin_for(model: &str) -> Option<ModelPricing> {
    // lowercase once so every pattern comparison is case-insensitive
    let model = model.to_ascii_lowercase();
    // the table is ordered most-specific first, so the first hit wins
    BUILTIN_PRICING
        .iter()
        .find(|(pattern, _)| boundary_match(&model, pattern))
        .map(|(_, pricing)| *pricing)
}

/// Resolves the effective pricing for a model from the table and config overrides
///
/// A built-in entry seeds the rates and any override field replaces its rate.
/// Without a built-in entry the overrides must carry at least `input` and
/// `output` to be usable; the cache rates then default to the `input` rate --
/// a conservative estimate that never understates fresh-input cost. `None`
/// means no cost can be estimated and the display should hide its cost segment.
///
/// # Arguments
///
/// * `model` - The configured model id to price
/// * `overrides` - The config's optional pricing overrides for this model
pub fn resolve(model: &str, overrides: Option<&AiPricing>) -> Option<ModelPricing> {
    // seed from the built-in table when the model is recognized
    let base = builtin_for(model);
    match (base, overrides) {
        // a known model with overrides: replace just the overridden rates
        (Some(mut pricing), Some(overrides)) => {
            if let Some(input) = overrides.input {
                pricing.input = input;
            }
            if let Some(output) = overrides.output {
                pricing.output = output;
            }
            if let Some(cache_read) = overrides.cache_read {
                pricing.cache_read = cache_read;
            }
            if let Some(cache_write) = overrides.cache_write {
                pricing.cache_write = cache_write;
            }
            Some(pricing)
        }
        // a known model without overrides: the table rates stand
        (Some(pricing), None) => Some(pricing),
        // an unknown model is priceable only with input and output overrides
        (None, Some(overrides)) => {
            let input = overrides.input?;
            let output = overrides.output?;
            // default the cache rates to the input rate when not supplied
            Some(ModelPricing {
                input,
                output,
                cache_read: overrides.cache_read.unwrap_or(input),
                cache_write: overrides.cache_write.unwrap_or(input),
            })
        }
        // an unknown model with no overrides cannot be priced
        (None, None) => None,
    }
}

/// A resolved cost estimator for one provider + model pair
#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    /// The effective per-token prices for the model
    pub pricing: ModelPricing,
    /// Whether the provider's reported input tokens already include cache reads
    ///
    /// The OpenAI protocol reports `prompt_tokens` inclusive of cached tokens,
    /// while Anthropic/Bedrock report the uncached remainder only; the cost
    /// formula subtracts cache reads back out when this is set so cached input
    /// is never double-counted.
    pub input_includes_cached: bool,
}

impl CostModel {
    /// Builds a cost estimator for the active provider, if its model is priceable
    ///
    /// Takes the thorpal provider config (not the Thorctl one) because on
    /// resume the effective model is the one stored in the session snapshot.
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider config the session actually runs with
    /// * `overrides` - The config's optional pricing overrides for this model
    pub fn for_provider(
        provider: &AiProviderConfig,
        overrides: Option<&AiPricing>,
    ) -> Option<Self> {
        // the openai protocol folds cache reads into its reported input tokens;
        // note this also loses cache-write counts for anthropic models behind a
        // litellm gateway, slightly underestimating their cost
        let input_includes_cached = match provider {
            AiProviderConfig::OpenAi { .. } | AiProviderConfig::LiteLlm { .. } => true,
            AiProviderConfig::Anthropic { .. } | AiProviderConfig::Bedrock { .. } => false,
        };
        // resolve the model's effective pricing; an unpriceable model means no
        // estimator and the display hides its cost segment
        let pricing = resolve(provider.model(), overrides)?;
        Some(CostModel {
            pricing,
            input_includes_cached,
        })
    }

    /// Estimates the dollar cost of a run from its accumulated token counts
    ///
    /// # Arguments
    ///
    /// * `input` - The accumulated input tokens as the provider reported them
    /// * `output` - The accumulated output tokens
    /// * `cached` - The accumulated cache-read input tokens
    /// * `cache_creation` - The accumulated cache-write input tokens
    pub fn cost(&self, input: u64, output: u64, cached: u64, cache_creation: u64) -> f64 {
        // normalize to the fresh-input count: the openai protocol folds cache
        // reads into its reported input tokens, anthropic/bedrock do not
        let uncached = if self.input_includes_cached {
            input.saturating_sub(cached)
        } else {
            input
        };
        // price each bucket at its own rate, in dollars per million tokens
        let dollars = uncached as f64 * self.pricing.input
            + cached as f64 * self.pricing.cache_read
            + cache_creation as f64 * self.pricing.cache_write
            + output as f64 * self.pricing.output;
        dollars / 1_000_000.0
    }

    /// Estimates the dollars prompt caching saved (or cost) for a run
    ///
    /// Compares against a cache-less world where every one of these tokens
    /// would have been billed at the fresh-input rate: reads save their rate
    /// difference, writes cost their premium over plain ingestion. The result
    /// is independent of whether the provider folds cache reads into its
    /// reported input tokens, and a negative value means the cache cost more
    /// than it saved.
    ///
    /// # Arguments
    ///
    /// * `cached` - The accumulated cache-read input tokens
    /// * `cache_creation` - The accumulated cache-write input tokens
    pub fn cache_savings(&self, cached: u64, cache_creation: u64) -> f64 {
        // what the cached reads would have cost at the fresh-input rate
        let read_savings = cached as f64 * (self.pricing.input - self.pricing.cache_read);
        // what the cache writes cost beyond plain input ingestion
        let write_overhead =
            cache_creation as f64 * (self.pricing.cache_write - self.pricing.input);
        // net dollars, scaled from per-million-token rates
        (read_savings - write_overhead) / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table pattern resolves to its own entry, catching ordering bugs
    /// where a more-specific pattern would fall into a shorter prefix.
    #[test]
    fn every_builtin_pattern_resolves_to_itself() {
        for (pattern, pricing) in BUILTIN_PRICING {
            let resolved = builtin_for(pattern).expect("every pattern should self-resolve");
            assert_eq!(resolved, *pricing, "pattern {pattern} hit the wrong entry");
        }
    }

    /// Bedrock-prefixed, dated, and versioned ids all land on the right entry.
    #[test]
    fn prefixed_and_dated_ids_match() {
        // a fully dressed bedrock id resolves to its bare model's rates
        let bedrock = builtin_for("us.anthropic.claude-opus-4-6-20260205-v1:0").unwrap();
        assert_eq!(bedrock, builtin_for("claude-opus-4-6").unwrap());
        // a dated openai mini id resolves to the mini rates, not the base model
        let mini = builtin_for("gpt-4.1-mini-2026-01").unwrap();
        assert_eq!(mini, builtin_for("gpt-4.1-mini").unwrap());
        // the dotted generation resolves to its own rates, not its prefix
        assert_eq!(
            builtin_for("gpt-5.2").unwrap(),
            price(1.75, 0.175, 0.0, 14.0)
        );
        // the bedrock-prefixed mythos preview resolves to bedrock's higher rates
        assert_eq!(
            builtin_for("us.anthropic.claude-mythos-preview-v1:0").unwrap(),
            price(27.5, 2.75, 34.375, 137.5)
        );
        // the bare mythos preview keeps the first-party rates
        assert_eq!(
            builtin_for("claude-mythos-preview").unwrap(),
            price(10.0, 1.0, 12.5, 50.0)
        );
        // matching is case-insensitive
        assert!(builtin_for("Claude-Opus-5-5").is_some());
    }

    /// A pattern embedded in a longer alphanumeric run is not a match.
    #[test]
    fn embedded_ids_do_not_match() {
        // `o3` must not match inside unrelated words
        assert!(builtin_for("pro3-chat").is_none());
        // a model with no table entry resolves to nothing
        assert!(builtin_for("totally-unknown-model").is_none());
    }

    /// Overrides replace just the rates they name on a known model.
    #[test]
    fn overrides_patch_known_models() {
        // override only the output rate of a known model
        let overrides = AiPricing {
            input: None,
            output: Some(99.0),
            cache_read: None,
            cache_write: None,
        };
        let pricing = resolve("claude-opus-5-5", Some(&overrides)).unwrap();
        // the overridden rate changed and the others kept their table values
        assert_eq!(pricing.output, 99.0);
        assert_eq!(pricing.input, 4.0);
    }

    /// An unknown model is priceable only with both input and output overrides.
    #[test]
    fn overrides_price_unknown_models() {
        // a partial override (missing output) cannot price an unknown model
        let partial = AiPricing {
            input: Some(3.0),
            output: None,
            cache_read: None,
            cache_write: None,
        };
        assert!(resolve("my-litellm-alias", Some(&partial)).is_none());
        // a full input/output pair prices it, with cache rates defaulting to input
        let full = AiPricing {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: None,
            cache_write: None,
        };
        let pricing = resolve("my-litellm-alias", Some(&full)).unwrap();
        assert_eq!(pricing.cache_read, 3.0);
        assert_eq!(pricing.cache_write, 3.0);
        // and no overrides at all means no price
        assert!(resolve("my-litellm-alias", None).is_none());
    }

    /// The cost formula prices each bucket separately on the anthropic protocol.
    #[test]
    fn cost_prices_cache_buckets_separately() {
        // $1 input, $0.1 reads, $1.25 writes, $5 output per MTok
        let model = CostModel {
            pricing: price(1.0, 0.1, 1.25, 5.0),
            input_includes_cached: false,
        };
        // 1M of each bucket prices at exactly its own rate
        let dollars = model.cost(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert!((dollars - (1.0 + 5.0 + 0.1 + 1.25)).abs() < 1e-9);
    }

    /// The openai protocol's cache reads are subtracted back out of its input.
    #[test]
    fn cost_does_not_double_count_openai_cached_input() {
        // $1 input and $0.1 reads per MTok, inclusive input semantics
        let model = CostModel {
            pricing: price(1.0, 0.1, 0.0, 0.0),
            input_includes_cached: true,
        };
        // 100 input tokens of which 80 were cached: 20 fresh + 80 cached
        let dollars = model.cost(100, 0, 80, 0);
        assert!((dollars - (20.0 * 1.0 + 80.0 * 0.1) / 1_000_000.0).abs() < 1e-15);
    }

    /// Cache savings net read savings against write overhead on both protocols.
    #[test]
    fn cache_savings_nets_reads_against_writes() {
        // anthropic-style rates: $3 input, $0.3 reads, $3.75 writes per MTok
        let anthropic = CostModel {
            pricing: price(3.0, 0.3, 3.75, 15.0),
            input_includes_cached: false,
        };
        // 1M reads save $2.70; 1M writes cost $0.75 beyond plain ingestion
        let net = anthropic.cache_savings(1_000_000, 1_000_000);
        assert!((net - (2.7 - 0.75)).abs() < 1e-9);
        // writes alone make the cache a net cost, so the savings go negative
        let overhead = anthropic.cache_savings(0, 1_000_000);
        assert!((overhead + 0.75).abs() < 1e-9);
        // openai-style rates bill nothing for writes, so reads are pure savings
        let openai = CostModel {
            pricing: price(1.0, 0.1, 0.0, 0.0),
            input_includes_cached: true,
        };
        let reads = openai.cache_savings(1_000_000, 0);
        assert!((reads - 0.9).abs() < 1e-9);
    }
}
