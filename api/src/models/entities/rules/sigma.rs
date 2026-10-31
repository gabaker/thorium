//! A Sigma rule that can be applied to logs or log like data
use linearize::Linearize;
use sigma_rust::rule::Level;
use std::collections::BTreeMap;
use std::str::FromStr;

use crate::models::{Confidence, Flag, InvalidEnum};

/// The max number of actions a single sigma rule may have
///
/// Spawn reaction actions are validated by compiling their jq filters
/// synchronously at save time, so this bounds how much validation work one
/// rule save can cost and how large a stored rules action list can grow.
pub const MAX_SIGMA_ACTIONS: usize = 16;

/// The max number of images a single spawn reaction action may set args for
#[cfg(feature = "api")]
const MAX_SPAWN_IMAGES: usize = 16;

/// The max number of jq filters a single spawn reaction action may have
///
/// This counts positional and kwarg filters across all images in the action
/// since each filter is compiled synchronously at validation time.
#[cfg(feature = "api")]
const MAX_SPAWN_FILTERS: usize = 64;

/// The max number of switches a single spawn reaction action may have
///
/// This counts switches across all images in the action.
#[cfg(feature = "api")]
const MAX_SPAWN_SWITCHES: usize = 64;

/// The different kinds of data this rule should be run on
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, strum::Display, PartialEq, Eq, Hash, Linearize,
)]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreAsStr))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum SigmaRuleAppliesTo {
    /// Apply this rule to windows processes
    WindowsProcesses,
    /// Apply this rule to network connections
    NetworkConnections,
    /// Apply this rule to a compiled function
    CompiledFunctions,
    /// Apply this rule to a decompiled function
    DecompiledFunctions,
}

impl SigmaRuleAppliesTo {
    /// Convert this [`RuleAppliesTo`] into a str
    pub fn as_str(&self) -> &'static str {
        match self {
            SigmaRuleAppliesTo::WindowsProcesses => "WindowsProcesses",
            SigmaRuleAppliesTo::NetworkConnections => "NetworkConnections",
            SigmaRuleAppliesTo::CompiledFunctions => "CompiledFunctions",
            SigmaRuleAppliesTo::DecompiledFunctions => "DecompiledFunctions",
        }
    }
}

impl FromStr for SigmaRuleAppliesTo {
    type Err = InvalidEnum;

    /// Cast a str to an [`RuleAppliesTo`]
    ///
    /// # Arguments
    ///
    /// * `val` - The str to cast
    fn from_str(val: &str) -> Result<Self, Self::Err> {
        match val {
            "WindowsProcesses" => Ok(SigmaRuleAppliesTo::WindowsProcesses),
            "NetworkConnections" => Ok(SigmaRuleAppliesTo::NetworkConnections),
            "CompiledFunctions" => Ok(SigmaRuleAppliesTo::CompiledFunctions),
            "DecompiledFunctions" => Ok(SigmaRuleAppliesTo::DecompiledFunctions),
            _ => Err(InvalidEnum(format!("Unknown enum variant: {val}"))),
        }
    }
}

fn default_confidence_temp() -> Confidence {
    Confidence::Unsure
}

/// Automatically promote this sigma rule hit to a flag
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SigmaAutoFlag {
    /// How confident we are in this rule/flag not being a false positive
    #[serde(default = "default_confidence_temp")]
    pub confidence: Confidence,
    /// The interesting, odd, or suspicious characteristic
    pub content: Option<String>,
    /// The reason for this Flag
    pub reasoning: String,
}

impl SigmaAutoFlag {
    /// Build a flag with this info and its owner rule
    pub fn to_flag(&self, rule: &SigmaRule) -> Flag {
        Flag {
            suspicion: rule.score,
            confidence: self.confidence,
            content: self.content.clone(),
            reasoning: self.reasoning.clone(),
        }
    }
}

/// The args to build for a single image when spawning a reaction
///
/// Positional and kwarg values are jq filters that are run against the metadata
/// json of the entity that a sigma rule hit on while switches are passed through
/// as literal strings. Literal positional/kwarg values can be set with jq string
/// literals (e.g. `"\"memdump\""`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SigmaReactionArgs {
    /// The jq filters whose outputs become positional args in order
    #[serde(default)]
    pub positionals: Vec<String>,
    /// A map of kwarg names to the jq filters whose outputs become that kwargs values
    #[serde(default)]
    pub kwargs: BTreeMap<String, Vec<String>>,
    /// The literal switches to pass through unchanged
    #[serde(default)]
    pub switches: Vec<String>,
}

/// Spawn a reaction against the originating data when a sigma rule hits
///
/// The spawned reaction targets the same data (sample/repo) that the reaction
/// whose results this sigma rule hit on was originally ran against. Repo
/// backed results only record the repo url, so reactions spawned from them
/// run against the repos default checkout rather than the commit the
/// originating reaction ran against.
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SigmaSpawnReaction {
    /// The group the pipeline to spawn is in
    pub group: String,
    /// The pipeline to spawn
    pub pipeline: String,
    /// The args to overlay for images in this pipeline keyed by image name
    #[serde(default)]
    pub args: BTreeMap<String, SigmaReactionArgs>,
}

impl SigmaSpawnReaction {
    /// Create a new spawn reaction action
    ///
    /// # Arguments
    ///
    /// * `group` - The group the pipeline to spawn is in
    /// * `pipeline` - The pipeline to spawn
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::entities::rules::SigmaSpawnReaction;
    ///
    /// // dump the memory of any process this sigma rule hits on
    /// SigmaSpawnReaction::new("forensics", "volatility-dump")
    ///     .kwarg("volatility", "--pid", vec!(".pid"))
    ///     .positional("volatility", "\"memdump\"")
    ///     .switch("volatility", "--verbose");
    /// ```
    pub fn new<G: Into<String>, P: Into<String>>(group: G, pipeline: P) -> Self {
        SigmaSpawnReaction {
            group: group.into(),
            pipeline: pipeline.into(),
            args: BTreeMap::default(),
        }
    }

    /// Add a positional arg jq filter for an image
    ///
    /// # Arguments
    ///
    /// * `image` - The image to add a positional arg for
    /// * `filter` - The jq filter whose outputs become positional args
    #[must_use]
    pub fn positional<I: Into<String>, F: Into<String>>(mut self, image: I, filter: F) -> Self {
        // get or create this images args and add this positional filter
        self.args
            .entry(image.into())
            .or_default()
            .positionals
            .push(filter.into());
        self
    }

    /// Add a kwargs jq filters for an image
    ///
    /// # Arguments
    ///
    /// * `image` - The image to add a kwarg for
    /// * `key` - The kwarg to set values for
    /// * `filters` - The jq filters whose outputs become this kwargs values
    #[must_use]
    pub fn kwarg<I: Into<String>, K: Into<String>, F: Into<String>>(
        mut self,
        image: I,
        key: K,
        filters: Vec<F>,
    ) -> Self {
        // convert our filters to strings
        let converted = filters.into_iter().map(Into::into).collect();
        // get or create this images args and set this kwargs filters
        self.args
            .entry(image.into())
            .or_default()
            .kwargs
            .insert(key.into(), converted);
        self
    }

    /// Add a literal switch for an image
    ///
    /// # Arguments
    ///
    /// * `image` - The image to add a switch for
    /// * `switch` - The literal switch to pass through
    #[must_use]
    pub fn switch<I: Into<String>, S: Into<String>>(mut self, image: I, switch: S) -> Self {
        // get or create this images args and add this switch
        self.args
            .entry(image.into())
            .or_default()
            .switches
            .push(switch.into());
        self
    }

    /// Ensure this spawn reaction action is valid
    ///
    /// # Arguments
    ///
    /// * `filter` - The jq filter to validate
    #[cfg(feature = "api")]
    fn validate_filter(filter: &str) -> Result<(), crate::utils::ApiError> {
        // try to compile this jq filter
        if let Err(error) = super::jq::validate(filter) {
            // reject this action since this filter doesn't compile
            return crate::bad!(format!("Invalid jq filter '{filter}': {}", error.reason));
        }
        Ok(())
    }

    /// Ensure this spawn reaction action is valid
    ///
    /// This bounds checks the target group/pipeline/image names, bounds how
    /// many images/filters/switches one action may have, ensures all jq
    /// filters compile, and rejects empty kwargs/switches. The pipeline is
    /// intentionally not checked for existence since the user spawning reactions
    /// at hit time may differ from the rule author; missing pipelines are logged
    /// when a spawn is attempted instead.
    #[cfg(feature = "api")]
    pub fn validate(&self) -> Result<(), crate::utils::ApiError> {
        // bounds check our target group name with the same bounds as group creation
        crate::utils::bounder::string_lower(&self.group, "SpawnReaction['group']", 1, 50)?;
        // bounds check our target pipeline name with the same bounds as pipeline creation
        crate::utils::bounder::string_lower(&self.pipeline, "SpawnReaction['pipeline']", 1, 25)?;
        // bound how many images this action may set args for
        if self.args.len() > MAX_SPAWN_IMAGES {
            return crate::bad!(format!(
                "SpawnReaction actions may set args for at most {MAX_SPAWN_IMAGES} images"
            ));
        }
        // count this actions jq filters across all images before compiling any
        let filters: usize = self
            .args
            .values()
            .map(|args| args.positionals.len() + args.kwargs.values().map(Vec::len).sum::<usize>())
            .sum();
        // bound how many jq filters one action may compile at validation time
        if filters > MAX_SPAWN_FILTERS {
            return crate::bad!(format!(
                "SpawnReaction actions may have at most {MAX_SPAWN_FILTERS} jq filters"
            ));
        }
        // count this actions switches across all images
        let switches: usize = self.args.values().map(|args| args.switches.len()).sum();
        // bound how many switches one action may have
        if switches > MAX_SPAWN_SWITCHES {
            return crate::bad!(format!(
                "SpawnReaction actions may have at most {MAX_SPAWN_SWITCHES} switches"
            ));
        }
        // validate the args for each image in this spawn action
        for (image, args) in &self.args {
            // bounds check this image name with the same bounds as image creation
            crate::utils::bounder::string_lower(image, "SpawnReaction['image']", 1, 25)?;
            // validate each positional filter compiles
            for filter in &args.positionals {
                Self::validate_filter(filter)?;
            }
            // validate each kwarg and its filters
            for (key, filters) in &args.kwargs {
                // reject out of bounds kwarg names but allow any chars since
                // kwargs are arbitrary command line flags
                if key.is_empty() || key.len() > 256 {
                    return crate::bad!(format!(
                        "SpawnReaction kwargs must be between 1 and 256 chars: '{key}'"
                    ));
                }
                // reject kwargs with no filters since they would never set a value
                if filters.is_empty() {
                    return crate::bad!(format!(
                        "SpawnReaction kwarg '{key}' must have at least one jq filter"
                    ));
                }
                // validate each of this kwargs filters compile
                for filter in filters {
                    Self::validate_filter(filter)?;
                }
            }
            // reject out of bounds switches but allow any chars since switches
            // are arbitrary command line flags
            if let Some(switch) = args
                .switches
                .iter()
                .find(|switch| switch.is_empty() || switch.len() > 256)
            {
                return crate::bad!(format!(
                    "SpawnReaction switches must be between 1 and 256 chars: '{switch}'"
                ));
            }
        }
        Ok(())
    }
}

/// The action to take when a sigma rule hits
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
pub enum SigmaActionToTake {
    /// Automatically promote hit to a flag
    Flag(SigmaAutoFlag),
    /// Spawn a reaction against the data the matched entity came from
    SpawnReaction(SigmaSpawnReaction),
}

impl SigmaActionToTake {
    /// Ensure this actions config is valid
    #[cfg(feature = "api")]
    pub fn validate(&self) -> Result<(), crate::utils::ApiError> {
        // validate the correct kind of action
        match self {
            // flags have no config to validate beyond deserialization
            SigmaActionToTake::Flag(_) => Ok(()),
            // spawn reactions must have valid names and jq filters
            SigmaActionToTake::SpawnReaction(spawn) => spawn.validate(),
        }
    }
}

/// A Sigma rule that can be applied to logs or log like data
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct SigmaRule {
    /// The original unparsed rule
    pub rule: String,
    /// What types of data this sigma rule applies too
    pub applies_to: Vec<SigmaRuleAppliesTo>,
    /// The score to apply when this rule hits
    pub score: i64,
    /// The actions to take when this sigma rule hits
    #[serde(default)]
    pub actions: Vec<SigmaActionToTake>,
}

impl SigmaRule {
    /// Create a new [`SigmaRule`]
    ///
    /// # Arguments
    ///
    /// * `rule` - The sigma rule to create
    /// * `applies_to` - What this sigma rule applies too
    pub fn new(
        rule: impl Into<String>,
        applies_to: SigmaRuleAppliesTo,
    ) -> Result<Self, serde_norway::Error> {
        // convert this rule to a string
        let rule = rule.into();
        // parse our rule to make sure its valid
        let score = Self::validate_and_get_score(&rule)?;
        // build our validated sigma rule
        let validated = SigmaRule {
            rule,
            applies_to: vec![applies_to],
            score,
            actions: Vec::default(),
        };
        Ok(validated)
    }

    /// Update this sigma rules score
    ///
    /// # Arguments
    ///
    /// * `score` - The new score to set
    pub fn score(mut self, score: i64) -> Self {
        self.score = score;
        self
    }

    /// Update the action this sigma rule should take
    ///
    /// # Arguments
    ///
    /// * `action` - The action to take when this sigma rule hits
    pub fn action(mut self, action: SigmaActionToTake) -> Self {
        self.actions.push(action);
        self
    }

    /// Validate a sigma rule is valid and get its score
    ///
    /// # Arguments
    ///
    /// * `rule_str` - The sigma rule to parse and validate
    fn validate_and_get_score(rule_str: &str) -> Result<i64, serde_norway::Error> {
        // parse and validate this sigma rule
        let parsed = sigma_rust::rule_from_yaml(rule_str)?;
        // use a different score for each level
        match parsed.level {
            None | Some(Level::Informational) => Ok(0),
            Some(Level::Low) => Ok(1),
            Some(Level::Medium) => Ok(5),
            Some(Level::High) => Ok(20),
            Some(Level::Critical) => Ok(50),
        }
    }

    /// Create a new [`SigmaRule`] with the info in the form
    ///
    /// # Errors
    ///
    /// * A sigma rule was not found in the form
    ///
    /// # Arguments
    ///
    /// * `form` -  The metadata form
    #[cfg(feature = "api")]
    pub fn from_form(
        form: crate::models::entities::EntityMetadataForm,
    ) -> Result<Self, crate::utils::ApiError> {
        // if we don't have the rule field then return an error
        let rule = match form.sigma_rule {
            Some(rule) => rule,
            None => {
                return crate::bad!("Sigma rules must have a rule!".to_owned());
            }
        };
        // parse our rule to make sure its valid
        let level_score = Self::validate_and_get_score(&rule)?;
        // if we don't have a user supplied score then add our own based on the level for this rule
        let score = match form.score {
            // use the user supplied score
            Some(score) => score,
            None => level_score,
        };
        // bound how many actions a rule may have before validating any of them
        if form.sigma_actions.len() > MAX_SIGMA_ACTIONS {
            return crate::bad!(format!(
                "Sigma rules may have at most {MAX_SIGMA_ACTIONS} actions"
            ));
        }
        // make sure all of this rules actions are valid
        for action in &form.sigma_actions {
            // validate this actions config
            action.validate()?;
        }
        // build our windows process entity
        Ok(SigmaRule {
            rule,
            applies_to: form.sigma_applies_to,
            score,
            actions: form.sigma_actions,
        })
    }

    /// Add this [`SigmaRule`]'s metadata to a form
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
            .text("kind", crate::models::EntityKinds::SigmaRule.as_str())
            .text("metadata[sigma_rule]", self.rule)
            .text("metadata[score]", self.score.to_string());
        // add what data this sigma rule applies to (list fields require a trailing `[]`)
        let mut form =
            crate::multipart_list_conv!(form, "metadata[sigma_applies_to][]", self.applies_to);
        // add what actions to take when this sigma rule hits
        crate::multipart_list_serialize!(form, "metadata[sigma_actions][]", self.actions);
        Ok(form)
    }
}

#[cfg(all(test, feature = "api"))]
mod tests {
    use super::*;

    /// Get the message out of an api error for assertions
    ///
    /// # Arguments
    ///
    /// * `error` - The api error to get the message from
    fn error_msg(error: crate::utils::ApiError) -> String {
        // spawn action validation only returns Thorium errors with messages
        match error {
            crate::utils::ApiError::Thorium { msg, .. } => msg.unwrap_or_default(),
            other => panic!("Expected a Thorium api error: {other:?}"),
        }
    }

    /// A volatility like spawn action with valid jq filters validates
    #[test]
    fn validates_spawn_action() {
        // build a volatility like spawn action
        let action = SigmaActionToTake::SpawnReaction(
            SigmaSpawnReaction::new("forensics", "volatility-dump")
                .kwarg("volatility", "--pid", vec![".pid"])
                .positional("volatility", "\"memdump\"")
                .switch("volatility", "--verbose"),
        );
        // this action should be valid
        action.validate().expect("valid spawn action was rejected");
    }

    /// Flag actions always validate
    #[test]
    fn validates_flag_action() {
        // build a flag action
        let action = SigmaActionToTake::Flag(SigmaAutoFlag {
            confidence: Confidence::Unsure,
            content: None,
            reasoning: "sus".to_owned(),
        });
        // this action should be valid
        action.validate().expect("flag action was rejected");
    }

    /// Spawn actions with jq filters that don't compile are rejected
    #[test]
    fn rejects_bad_jq_filters() {
        // build a spawn action with a filter that doesn't compile
        let action = SigmaActionToTake::SpawnReaction(
            SigmaSpawnReaction::new("forensics", "volatility-dump").kwarg(
                "volatility",
                "--pid",
                vec!["]["],
            ),
        );
        // this action should be rejected
        let error = action.validate().expect_err("bad jq filter was accepted");
        // the error should name the offending filter
        assert!(error_msg(error).contains("]["));
    }

    /// Spawn actions with out of bounds group/pipeline names are rejected
    #[test]
    fn rejects_bad_names() {
        // empty group names are rejected
        let empty_group = SigmaSpawnReaction::new("", "volatility-dump");
        assert!(empty_group.validate().is_err());
        // empty pipeline names are rejected
        let empty_pipeline = SigmaSpawnReaction::new("forensics", "");
        assert!(empty_pipeline.validate().is_err());
        // uppercase pipeline names are rejected like at pipeline creation
        let upper = SigmaSpawnReaction::new("forensics", "Volatility");
        assert!(upper.validate().is_err());
    }

    /// Spawn actions with bad image names, kwargs, or switches are rejected
    #[test]
    fn rejects_bad_args() {
        // empty image names are rejected
        let empty_image = SigmaSpawnReaction::new("forensics", "volatility-dump").kwarg(
            "",
            "--pid",
            vec![".pid"],
        );
        assert!(empty_image.validate().is_err());
        // empty kwarg names are rejected
        let empty_kwarg = SigmaSpawnReaction::new("forensics", "volatility-dump").kwarg(
            "volatility",
            "",
            vec![".pid"],
        );
        assert!(empty_kwarg.validate().is_err());
        // kwargs with no filters are rejected
        let no_filters = SigmaSpawnReaction::new("forensics", "volatility-dump").kwarg(
            "volatility",
            "--pid",
            Vec::<String>::new(),
        );
        assert!(no_filters.validate().is_err());
        // empty switches are rejected
        let empty_switch =
            SigmaSpawnReaction::new("forensics", "volatility-dump").switch("volatility", "");
        assert!(empty_switch.validate().is_err());
    }

    /// Spawn actions past the image/filter/switch caps are rejected
    #[test]
    fn rejects_oversized_actions() {
        // build a spawn action with one image too many
        let mut too_many_images = SigmaSpawnReaction::new("forensics", "volatility-dump");
        for index in 0..=MAX_SPAWN_IMAGES {
            too_many_images = too_many_images.switch(format!("image{index}"), "--verbose");
        }
        assert!(too_many_images.validate().is_err());
        // build a spawn action with one jq filter too many
        let mut too_many_filters = SigmaSpawnReaction::new("forensics", "volatility-dump");
        for _ in 0..=MAX_SPAWN_FILTERS {
            too_many_filters = too_many_filters.positional("volatility", ".pid");
        }
        assert!(too_many_filters.validate().is_err());
        // build a spawn action with one switch too many
        let mut too_many_switches = SigmaSpawnReaction::new("forensics", "volatility-dump");
        for _ in 0..=MAX_SPAWN_SWITCHES {
            too_many_switches = too_many_switches.switch("volatility", "--verbose");
        }
        assert!(too_many_switches.validate().is_err());
    }
}
