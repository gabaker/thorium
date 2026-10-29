//! Keys for accessing hunt data

use uuid::Uuid;

use crate::utils::Shared;

/// Keys to use to access hunt data
pub struct HuntKeys;

impl HuntKeys {
    /// Builds the key one hunt lane's checkpoint sequence lives at
    ///
    /// The checkpoint route's compare-and-set runs against this key; the
    /// entity row only persists a view of it. If Redis loses the key it is
    /// reseeded from the entity's last persisted sequence.
    ///
    /// # Arguments
    ///
    /// * `hunt` - The hunt's entity id
    /// * `lane` - The lane index this sequence is for
    /// * `shared` - Shared Thorium objects
    pub fn lane_seq(hunt: &Uuid, lane: u32, shared: &Shared) -> String {
        format!(
            "{ns}:hunts:{hunt}:lane:{lane}:seq",
            ns = shared.config.thorium.namespace
        )
    }

    /// Builds the key for one hunt's entity persist mutex
    ///
    /// Concurrent lane checkpoints, cancels, and deletes rewrite the whole
    /// hunt metadata blob last-write-wins, so every writer serializes its
    /// read-modify-write through this short lived lock.
    ///
    /// # Arguments
    ///
    /// * `hunt` - The hunt's entity id
    /// * `shared` - Shared Thorium objects
    pub fn persist(hunt: &Uuid, shared: &Shared) -> String {
        format!(
            "{ns}:hunts:{hunt}:persist",
            ns = shared.config.thorium.namespace
        )
    }
}
