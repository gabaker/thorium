//! A YARA rule that can be applied to samples
use linearize::Linearize;
// the ScyllaStoreAsStr derive expands to a call through the FromStr trait
#[cfg(feature = "scylla-utils")]
use std::str::FromStr;

use crate::models::{Confidence, EntityKinds, Flag, InvalidEnum};

/// The different kinds of data this rule should be run on
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::AsRefStr,
    strum::EnumIter,
    PartialEq,
    Eq,
    Hash,
    Linearize,
)]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreAsStr))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum YaraRuleAppliesTo {
    /// Apply this rule to samples
    Samples,
}

impl YaraRuleAppliesTo {
    /// Convert this [`YaraRuleAppliesTo`] into a str
    pub fn as_str(&self) -> &str {
        // use the strum generated str conversion so new variants cannot be missed
        self.as_ref()
    }
}

/// Help serde default the confidence for a yara auto flag
fn default_confidence_temp() -> Confidence {
    Confidence::Unsure
}

/// Automatically promote this yara rule hit to a flag
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct YaraAutoFlag {
    /// How confident we are in this rule/flag not being a false positive
    #[serde(default = "default_confidence_temp")]
    pub confidence: Confidence,
    /// The interesting, odd, or suspicious characteristic
    pub content: Option<String>,
    /// The reason for this Flag
    pub reasoning: String,
}

impl YaraAutoFlag {
    /// Build a flag with this info and its owner rule
    ///
    /// # Arguments
    ///
    /// * `rule` - The yara rule this auto flag is for
    pub fn to_flag(&self, rule: &YaraRule) -> Flag {
        Flag {
            suspicion: rule.score,
            confidence: self.confidence,
            content: self.content.clone(),
            reasoning: self.reasoning.clone(),
        }
    }
}

/// The action to take when a yara rule hits
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
pub enum YaraActionToTake {
    /// Automatically promote hit to a flag
    Flag(YaraAutoFlag),
}

/// A YARA rule that can be applied to samples
///
/// A single yara rule entity carries one yara source document which may define
/// several rules. Splitting multi rule documents would break private rules and
/// cross rule conditions which only resolve within one source.
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct YaraRule {
    /// The raw yara source document (may define several rules)
    pub rule: String,
    /// What types of data this yara rule applies too
    pub applies_to: Vec<YaraRuleAppliesTo>,
    /// The score to apply when this rule hits
    pub score: i64,
    /// The actions to take when this yara rule hits
    #[serde(default)]
    pub actions: Vec<YaraActionToTake>,
    /// Whether this rule is exempt from noisy/slow quarantine
    ///
    /// This is settable only by admins through the dedicated quarantine
    /// exemption route; the normal update path never touches it. It is not
    /// part of rule identity so flipping it leaves `rule_hash` and the
    /// scanners serialized ruleset cache keys unchanged.
    #[serde(default)]
    pub quarantine_exempt: bool,
}

impl YaraRule {
    /// The hard cap on yara rule source size in bytes (128 KiB)
    ///
    /// Clients enforce this constant since they have no access to a Thorium
    /// config while the server re-checks against `thorium.yara.max_rule_size`.
    pub const MAX_RULE_SIZE: usize = 131_072;

    /// The entity tag key quarantine status is written under
    ///
    /// The event handler's scanner tags rules with this key when it
    /// quarantines them so consumers like the hunt snapshot filter must test
    /// the exact same key and values.
    pub const STATUS_TAG: &str = "YaraRuleStatus";

    /// The status tag value for rules quarantined as too noisy
    pub const STATUS_NOISY: &str = "Noisy";

    /// The status tag value for rules quarantined as too slow
    pub const STATUS_SLOW: &str = "Slow";

    /// The status tag value for rules that fail to compile
    ///
    /// Unlike `Noisy`/`Slow` this status is not cleared by quarantine
    /// exemptions: a broken rule cannot scan no matter how trusted it is.
    pub const STATUS_BROKEN: &str = "Broken";

    /// Create a new [`YaraRule`]
    ///
    /// This only performs pure checks (non-empty source under
    /// [`YaraRule::MAX_RULE_SIZE`]) since clients cannot compile rules; the
    /// server compiles and validates rules on both create and update.
    ///
    /// # Arguments
    ///
    /// * `rule` - The yara rule to create
    /// * `applies_to` - What this yara rule applies too
    pub fn new(
        rule: impl Into<String>,
        applies_to: YaraRuleAppliesTo,
    ) -> Result<Self, InvalidEnum> {
        // convert this rule to a string
        let rule = rule.into();
        // make sure this rule actually has a body
        if rule.is_empty() {
            return Err(InvalidEnum("Yara rules cannot be empty!".to_owned()));
        }
        // make sure this rule is under our hard size cap
        if rule.len() > Self::MAX_RULE_SIZE {
            return Err(InvalidEnum(format!(
                "Yara rules cannot be larger then {} bytes!",
                Self::MAX_RULE_SIZE
            )));
        }
        // build our yara rule
        let built = YaraRule {
            rule,
            applies_to: vec![applies_to],
            score: 0,
            actions: Vec::default(),
            quarantine_exempt: false,
        };
        Ok(built)
    }

    /// Update this yara rules score
    ///
    /// # Arguments
    ///
    /// * `score` - The new score to set
    pub fn score(mut self, score: i64) -> Self {
        self.score = score;
        self
    }

    /// Update the action this yara rule should take
    ///
    /// # Arguments
    ///
    /// * `action` - The action to take when this yara rule hits
    pub fn action(mut self, action: YaraActionToTake) -> Self {
        self.actions.push(action);
        self
    }

    /// Get the content hash for this rules source
    ///
    /// This hash is recorded on every hit so a hit stays attributable to the
    /// exact rule text that fired even after the rule is edited.
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn rule_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        // build a hasher for this rules source
        let mut hasher = Sha256::new();
        // hash this rules source
        hasher.update(self.rule.as_bytes());
        // encode this hash as lowercase hex
        data_encoding::HEXLOWER.encode(&hasher.finalize())
    }

    /// Validate a yara rule compiles and respects this Thorium instances limits
    ///
    /// Validation happens in this order: emptiness, size against
    /// `thorium.yara.max_rule_size`, include statement rejection, module import
    /// allowlisting, and finally a single yara-x compile pass. Compiler errors
    /// and warnings are both surfaced in the returned 400 so users can fix
    /// their rule without resubmitting blind. This is called on both the create
    /// and update paths so stored rules are always compilable at write time.
    ///
    /// # Errors
    ///
    /// * The rule is empty or larger then `thorium.yara.max_rule_size`
    /// * The rule contains an include statement
    /// * The rule imports a module outside `thorium.yara.allowed_modules`
    /// * The rule fails to compile with yara-x
    ///
    /// # Arguments
    ///
    /// * `rule` - The yara rule source to validate
    /// * `conf` - The yara settings for this Thorium instance
    #[cfg(all(feature = "api", feature = "yara-x"))]
    pub fn validate(
        rule: &str,
        conf: &crate::conf::Yara,
    ) -> Result<yara_x::Rules, crate::utils::ApiError> {
        // make sure this rule actually has a body
        if rule.is_empty() {
            return crate::bad!("Yara rules cannot be empty!".to_owned());
        }
        // make sure this rule is under this instances max rule size
        if rule.len() > conf.max_rule_size {
            return crate::bad!(format!(
                "Yara rules cannot be larger then {} bytes!",
                conf.max_rule_size
            ));
        }
        // scan this rules lines for include statements and disallowed module imports
        for line in rule.lines() {
            // ignore any leading whitespace on this line
            let trimmed = line.trim_start();
            // reject include statements since the API has no rule filesystem to include from
            if let Some(rest) = trimmed.strip_prefix("include") {
                // only treat this as an include statement if the keyword ends here
                if rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('"')
                {
                    return crate::bad!("Yara rules cannot contain include statements!".to_owned());
                }
            }
            // check any module imports against this instances allowlist
            if let Some(rest) = trimmed.strip_prefix("import") {
                // only treat this as an import statement if the keyword ends here
                if rest.starts_with(char::is_whitespace) || rest.starts_with('"') {
                    // extract the quoted module name from this import statement
                    if let Some(module) = rest
                        .trim_start()
                        .strip_prefix('"')
                        .and_then(|inner| inner.split('"').next())
                    {
                        // make sure this module is in our allowlist
                        if !conf.allowed_modules.iter().any(|allowed| allowed == module) {
                            return crate::bad!(format!(
                                "Yara module '{module}' is not allowed! Allowed modules: {:?}",
                                conf.allowed_modules
                            ));
                        }
                    }
                }
            }
        }
        // build the compiler to validate this rule with
        let mut compiler = yara_x::Compiler::new();
        // disable includes so anything that slips past the line scan still fails to compile
        compiler.enable_includes(false);
        // compile this rules source in a single pass
        if compiler.add_source(rule).is_err() {
            // gather all of the errors this rule caused
            let errors = compiler
                .errors()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>();
            // gather all of the warnings this rule caused
            let warnings = compiler
                .warnings()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>();
            // surface both the errors and warnings so users can fix their rule
            return crate::bad!(format!(
                "Yara rule failed to compile: errors: {errors:#?} warnings: {warnings:#?}"
            ));
        }
        // build and return our compiled rules
        Ok(compiler.build())
    }

    /// Create a new [`YaraRule`] with the info in the form
    ///
    /// # Errors
    ///
    /// * A yara rule was not found in the form
    /// * The yara rule failed validation
    ///
    /// # Arguments
    ///
    /// * `form` - The metadata form
    /// * `conf` - The config for this Thorium instance
    #[cfg(feature = "api")]
    pub fn from_form(
        form: crate::models::entities::EntityMetadataForm,
        conf: &crate::conf::Conf,
    ) -> Result<Self, crate::utils::ApiError> {
        // if we don't have the rule field then return an error
        let rule = match form.yara_rule {
            Some(rule) => rule,
            None => {
                return crate::bad!("Yara rules must have a rule!".to_owned());
            }
        };
        // compile our rule to make sure its valid
        Self::validate(&rule, &conf.thorium.yara)?;
        // yara rules have no level info so default the score to 0 if one wasn't supplied
        let score = form.score.unwrap_or_default();
        // build our yara rule entity; exemption is admin-set after creation
        Ok(YaraRule {
            rule,
            applies_to: form.yara_applies_to,
            score,
            actions: form.yara_actions,
            quarantine_exempt: false,
        })
    }

    /// Add this [`YaraRule`]'s metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        mut self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // always set our entity kind
        let form = form
            .text("kind", EntityKinds::YaraRule.as_str())
            .text("metadata[yara_rule]", self.rule)
            .text("metadata[score]", self.score.to_string());
        // add what data this yara rule applies to (list fields require a trailing `[]`)
        let mut form =
            crate::multipart_list_conv!(form, "metadata[yara_applies_to][]", self.applies_to);
        // add what actions to take when this yara rule hits
        crate::multipart_list_serialize!(form, "metadata[yara_actions][]", self.actions);
        Ok(form)
    }
}

impl From<YaraRuleAppliesTo> for EntityKinds {
    /// Map what a yara rule applies to to an entity kind
    ///
    /// # Arguments
    ///
    /// * `applies_to` - The yara applies to to map
    fn from(applies_to: YaraRuleAppliesTo) -> Self {
        // map what a yara rule applies to to an entity kind
        match applies_to {
            // samples are not entities so there is no entity kind to scan;
            // the yara scanner reads files directly instead of crawling entities
            YaraRuleAppliesTo::Samples => EntityKinds::Other,
        }
    }
}

impl From<&YaraRuleAppliesTo> for EntityKinds {
    /// Map what a yara rule applies to to an entity kind by ref
    ///
    /// # Arguments
    ///
    /// * `applies_to` - The yara applies to to map
    fn from(applies_to: &YaraRuleAppliesTo) -> Self {
        EntityKinds::from(*applies_to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All applies to variants must round trip through `as_str` and `FromStr`
    ///
    /// This guards against the hand-written `FromStr` trap that left
    /// `SigmaRuleAppliesTo` only able to parse 2 of its 4 variants.
    #[test]
    fn applies_to_all_variants_parse() {
        use std::str::FromStr;
        use strum::IntoEnumIterator;
        // step over every applies to variant
        for variant in YaraRuleAppliesTo::iter() {
            // round trip this variant through its str form
            let parsed = YaraRuleAppliesTo::from_str(variant.as_str())
                .expect("failed to parse an applies to variant from its own str form");
            // make sure we got the same variant back
            assert_eq!(parsed, variant);
        }
    }

    /// The rule content hash must stay pinned since hits reference it forever
    #[cfg(any(feature = "api", feature = "client"))]
    #[test]
    fn rule_hash_stable() {
        // build a yara rule with a fixed source
        let rule = YaraRule::new("rule a { condition: true }", YaraRuleAppliesTo::Samples)
            .expect("failed to build yara rule");
        // make sure the content hash matches its pinned vector
        assert_eq!(
            rule.rule_hash(),
            "a378d9b9c867ac04ad902df80e0c53f04fb3ceee51d164bc650eb247a80b907e"
        );
    }
}

#[cfg(all(test, feature = "api", feature = "yara-x"))]
mod compile_tests {
    use super::*;
    use crate::conf::Yara;
    use crate::utils::ApiError;

    /// A minimal valid yara rule with meta, strings, and a condition
    const VALID_RULE: &str = r#"import "pe"

rule ThoriumUnitTest
{
    meta:
        author = "thorium-tests"
        description = "A unit test rule"
    strings:
        $marker = "thorium-yara-test-marker"
    condition:
        $marker or pe.is_pe
}
"#;

    /// Assert that a validation result is a 400 whose message contains a needle
    ///
    /// # Arguments
    ///
    /// * `result` - The validation result to check
    /// * `needle` - The substring the error message must contain
    fn assert_bad_request(result: Result<yara_x::Rules, ApiError>, needle: &str) {
        // make sure validation failed with a 400 mentioning our needle
        match result {
            Err(ApiError::Thorium { code, msg }) => {
                // make sure this is a 400
                assert_eq!(code, axum::http::StatusCode::BAD_REQUEST);
                // make sure the message mentions what went wrong
                let msg = msg.expect("400 error is missing a message");
                assert!(
                    msg.contains(needle),
                    "error message '{msg}' does not contain '{needle}'"
                );
            }
            Err(other) => panic!("expected a 400 Thorium error but got {other:#?}"),
            Ok(_) => panic!("expected validation to fail but it passed"),
        }
    }

    /// A valid rule with an allowlisted import must compile
    #[test]
    fn valid_rule_compiles() {
        // validate a known good rule against the default yara settings
        let compiled = YaraRule::validate(VALID_RULE, &Yara::default())
            .expect("a valid yara rule failed validation");
        // make sure our rule actually made it into the compiled set
        assert_eq!(compiled.iter().count(), 1);
    }

    /// A rule with broken syntax must be rejected with a 400
    #[test]
    fn syntax_error_rejected() {
        // validate a rule that cannot possibly compile
        let result = YaraRule::validate("rule broken {", &Yara::default());
        // make sure the compile failure surfaced as a 400
        assert_bad_request(result, "failed to compile");
    }

    /// A rule with an include statement must be rejected with a 400
    #[test]
    fn include_rejected() {
        // build a rule that tries to include another file
        let rule = "include \"other.yar\"\nrule a { condition: true }";
        // validate our include carrying rule
        let result = YaraRule::validate(rule, &Yara::default());
        // make sure the include was rejected
        assert_bad_request(result, "include");
    }

    /// A rule importing a module outside the allowlist must be rejected
    #[test]
    fn unknown_module_rejected() {
        // build a rule that imports a module that is not allowlisted
        let rule = "import \"cuckoo\"\nrule a { condition: true }";
        // validate our disallowed import carrying rule
        let result = YaraRule::validate(rule, &Yara::default());
        // make sure the import was rejected by the allowlist
        assert_bad_request(result, "cuckoo");
    }

    /// A rule over the configured max size must be rejected with a 400
    #[test]
    fn oversize_rejected() {
        // build yara settings with a tiny max rule size
        let conf = Yara {
            max_rule_size: 16,
            ..Yara::default()
        };
        // validate a rule that is over our tiny size cap
        let result = YaraRule::validate("rule a { condition: true }", &conf);
        // make sure the oversize rule was rejected
        assert_bad_request(result, "larger");
    }
}
