//! The different rules that Thorium can apply to data

mod sigma;
mod yara;

pub use sigma::{SigmaActionToTake, SigmaAutoFlag, SigmaRule, SigmaRuleAppliesTo};
pub use yara::{YaraActionToTake, YaraAutoFlag, YaraRule, YaraRuleAppliesTo};
