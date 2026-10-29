//! Contains models for rule set entities

use uuid::Uuid;

use crate::models::Entity;

#[cfg(feature = "client")]
use crate::multipart_list_conv;

/// A named group of associated rules of any engine
///
/// Membership is stored as `RuleSetMember` associations to the member rule
/// entities (the `Device -> DevelopedBy -> vendor` pattern); this struct
/// carries only the read-time hydrated member list, which is stripped before
/// every persist exactly like `Device.vendors`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct RuleSet {
    /// The member rules, hydrated from associations at read time
    #[serde(default)]
    #[cfg_attr(feature = "api", schema(no_recursion))]
    pub rules: Vec<Entity>,
}

/// A request to create a rule set entity
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct RuleSetRequest {
    /// The ids of the rule entities that are members of this rule set
    #[serde(default)]
    pub rules: Vec<Uuid>,
}

impl RuleSetRequest {
    /// Create a new rule set request from a list of member rule ids
    ///
    /// # Arguments
    ///
    /// * `rules` - The ids of the rule entities to add as members
    #[must_use]
    pub fn new(rules: Vec<Uuid>) -> Self {
        RuleSetRequest { rules }
    }

    /// Add this rule set entity metadata to a form
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
        let form = form.text("kind", super::EntityKinds::RuleSet.as_str());
        // add our member rule ids
        let form = multipart_list_conv!(form, "metadata[ruleset_rules][]", self.rules);
        Ok(form)
    }
}
