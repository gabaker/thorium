//! The different rules that Thorium can apply to data

#[cfg(feature = "jq")]
pub mod jq;
mod sigma;
mod yara;

pub use sigma::{
    MAX_SIGMA_ACTIONS, SigmaActionToTake, SigmaAutoFlag, SigmaReactionArgs, SigmaRule,
    SigmaRuleAppliesTo, SigmaSpawnReaction,
};
pub use yara::{YaraActionToTake, YaraAutoFlag, YaraRule, YaraRuleAppliesTo};
