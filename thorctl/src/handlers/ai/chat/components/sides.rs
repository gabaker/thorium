//! The shared registry of side conversations the TUI's btw tabs render from.
//!
//! A side conversation (`/btw <name>`) has its own chat log and its own
//! token-usage bucket, separate from the main conversation's. The worker tasks
//! write into a side's handle as its events arrive, and the side's tab renders
//! from the same handle; get-or-create is idempotent so the worker and the UI
//! can race to materialize a side without ordering constraints.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::handlers::ai::chat::ChatContext;

use super::RunActivity;

/// The per-side display handles one btw tab renders from.
#[derive(Clone, Default)]
pub struct SideHandle {
    /// The side conversation's own chat log.
    pub context: ChatContext,
    /// The side conversation's own activity/usage bucket.
    pub activity: RunActivity,
}

/// The shared registry of side conversations, keyed by their names.
///
/// Cloning shares the same underlying registry (it is an `Arc` internally), so
/// the worker tasks' clones and the tabs' clones always observe the same sides.
#[derive(Clone, Default)]
pub struct SideRegistry(Arc<Mutex<BTreeMap<String, SideHandle>>>);

impl SideRegistry {
    /// Fetches a side's handle, creating an empty one when it does not exist
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation's name
    pub fn handle(&self, name: &str) -> SideHandle {
        // take the lock and get-or-create the side's shared handle
        self.0
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    /// Drops a closed side's handle from the registry
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation's name
    pub fn remove(&self, name: &str) {
        // take the lock and drop the side's entry
        self.0.lock().unwrap().remove(name);
    }

    /// The names of every registered side, in stable sorted order
    pub fn names(&self) -> Vec<String> {
        // the map is a BTreeMap, so its keys iterate in sorted order
        self.0.lock().unwrap().keys().cloned().collect()
    }

    /// Whether any side conversation currently has a turn in flight
    ///
    /// The app's redraw ticker reads this so a side turn keeps the throbber
    /// animating even while the main conversation sits idle.
    pub fn any_busy(&self) -> bool {
        // snapshot each side's activity, stopping at the first busy one
        self.0
            .lock()
            .unwrap()
            .values()
            .any(|handle| handle.activity.snapshot().busy)
    }
}

/// Extracts the side-conversation scope from an agent display name
///
/// A side's root agent is named `btw:<name>` and sub-agents spawned under it
/// are named `btw:<name>/Role(Moon)`, so the scope is everything between the
/// `btw:` prefix and the first `/`. A name without the prefix belongs to the
/// main conversation.
///
/// # Arguments
///
/// * `agent` - The agent display name to extract the scope from
pub fn scope_of_agent(agent: &str) -> Option<&str> {
    // only scoped names carry the btw prefix
    let scoped = agent.strip_prefix("btw:")?;
    // the scope ends at the first slash when a sub-agent suffix follows
    match scoped.split_once('/') {
        Some((scope, _)) => Some(scope),
        None => Some(scoped),
    }
}

/// True when the display name is a side conversation's own root agent
///
/// A side root (`btw:<name>` with no sub-agent suffix) delivers its final
/// answer through the prompt task, so its per-turn text is skipped from the
/// chat log exactly like the main agent's.
///
/// # Arguments
///
/// * `agent` - The agent display name to test
pub fn is_side_root(agent: &str) -> bool {
    // a root carries the prefix but no sub-agent suffix
    agent.starts_with("btw:") && !agent.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scopes parse out of root and sub-agent names, and main names have none
    #[test]
    fn scope_of_agent_parses_names() {
        // a side root's scope is everything after the prefix
        assert_eq!(scope_of_agent("btw:crypto"), Some("crypto"));
        // a scoped sub-agent's scope ends at the first slash
        assert_eq!(
            scope_of_agent("btw:crypto/ReverseEngineer(Europa)"),
            Some("crypto")
        );
        // a scoped swarm parses the same way
        assert_eq!(scope_of_agent("btw:crypto/Swarm(Io)"), Some("crypto"));
        // main-conversation names carry no scope at all
        assert_eq!(scope_of_agent("main"), None);
        assert_eq!(scope_of_agent("ReverseEngineer(Europa)"), None);
    }

    /// Only unsuffixed btw names are side roots
    #[test]
    fn is_side_root_requires_bare_prefix() {
        // the side's own root agent matches
        assert!(is_side_root("btw:crypto"));
        // its sub-agents and everything else do not
        assert!(!is_side_root("btw:crypto/ReverseEngineer(Europa)"));
        assert!(!is_side_root("main"));
        assert!(!is_side_root("ReverseEngineer(Europa)"));
    }

    /// The registry get-or-creates shared handles and forgets removed sides
    #[test]
    fn registry_handles_are_shared() {
        let registry = SideRegistry::default();
        // the first fetch creates the side's handle
        let handle = registry.handle("crypto");
        handle.context.add_user("hello".to_owned());
        // a second fetch shares the same underlying log
        assert_eq!(registry.handle("crypto").context.access().len(), 1);
        assert_eq!(registry.names(), ["crypto"]);
        // removing the side forgets its state entirely
        registry.remove("crypto");
        assert!(registry.names().is_empty());
        assert!(registry.handle("crypto").context.access().is_empty());
    }
}
