//! Backend logic for detection rule entities
//!
//! Rules mostly ride the generic entity paths; this module holds the rule
//! specific operations that cannot, like the admin only quarantine exemption
//! which must never be reachable through the group editor update surface.

use tracing::instrument;

use crate::models::backends::TagSupport;
use crate::models::backends::db;
use crate::models::{Entity, EntityMetadata, TagDeleteRequest, User, YaraRule};
use crate::utils::{ApiError, Shared};
use crate::{bad, is_admin};

impl Entity {
    /// Set or clear a yara rules quarantine exemption
    ///
    /// Only admins may manage exemptions since any group editor could
    /// otherwise shield their own noisy rules from quarantine. The flag is
    /// persisted first so a failure clearing stale quarantine tags still
    /// leaves the rule protected; setting the exemption also deletes any
    /// `Noisy`/`Slow` quarantine status values so the rule rejoins scanning
    /// in the same action (`Broken` is deliberately left alone since a rule
    /// that no longer compiles cannot scan regardless). Both writes bump the
    /// rule cache generation so the event handlers converge on their own.
    ///
    /// # Arguments
    ///
    /// * `user` - The user managing this rules exemption
    /// * `exempt` - Whether this rule should be exempt from quarantine
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "Entity::set_quarantine_exemption",
        skip(self, user, shared),
        fields(id = self.id.to_string(), exempt),
        err(Debug)
    )]
    pub async fn set_quarantine_exemption(
        mut self,
        user: &User,
        exempt: bool,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only admins may manage quarantine exemptions
        is_admin!(user);
        // only yara rules can be exempted since quarantine is yara only
        let EntityMetadata::YaraRule(rule) = &mut self.metadata else {
            return bad!(format!("Entity {} is not a yara rule!", self.id));
        };
        // set the exemption flag on this rules metadata
        rule.quarantine_exempt = exempt;
        // capture the noisy/slow status values present before we persist
        let stale: Vec<String> =
            self.tags
                .get(YaraRule::STATUS_TAG)
                .map_or_else(Vec::new, |values| {
                    // keep only the values exemption is allowed to clear
                    values
                        .keys()
                        .filter(|value| {
                            value.as_str() == YaraRule::STATUS_NOISY
                                || value.as_str() == YaraRule::STATUS_SLOW
                        })
                        .cloned()
                        .collect()
                });
        // persist the flag first so a tag delete failure below still leaves
        // this rule protected; retrying the idempotent route finishes the clear
        db::entities::update(user, self.clone(), &[], &[], shared).await?;
        // setting an exemption also re-admits the rule in the same action
        if exempt && !stale.is_empty() {
            // scope the delete to every group this rule lives in
            let mut req = TagDeleteRequest::<Entity>::default().groups(self.groups.clone());
            // delete every noisy/slow status value this rule carries
            for value in &stale {
                req.add_ref(YaraRule::STATUS_TAG, value);
            }
            // delete the stale quarantine status values
            self.delete_tags(user, req, shared).await?;
        }
        Ok(())
    }
}
