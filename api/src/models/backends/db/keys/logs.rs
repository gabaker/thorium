use crate::models::StatusUpdate;
use crate::utils::Shared;

/// Builds key to log queue
///
/// # Arguments
///
/// * `cast` - A status update for a reaction
/// * `shared` - Shared Thorium objects
pub fn queue_name(cast: &StatusUpdate, shared: &Shared) -> String {
    // base key to build the queue key off of
    format!(
        "{ns}:logs:{group}:{reaction}",
        ns = shared.config.thorium.namespace,
        group = cast.group,
        reaction = cast.reaction
    )
}
