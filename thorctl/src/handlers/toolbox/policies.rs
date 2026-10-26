//! Network policy transport for toolboxes
//!
//! Images and tools reference network policies by name, so a toolbox that doesn't carry
//! the policy definitions imports broken on instances that lack them. This module
//! classifies a bundled policy against the target instance and creates, or when asked
//! updates, the policies an import needs before the tools that use them.
//!
//! By default policies are not updated, even under `--overwrite`: they are
//! cluster security state owned by instance admins, so an existing-but-different
//! policy only produces a warning. Passing `--update-network-policy` opts into
//! pushing the toolbox's definition: rule/flag drift overwrites the existing
//! policy and a groups-only difference adds the toolbox's groups to it (group
//! coverage is only ever added, never removed).

use http::StatusCode;
use std::collections::HashMap;
use thorium::models::{
    IpBlockRaw, NetworkPolicy, NetworkPolicyListOpts, NetworkPolicyRequest, NetworkPolicyRule,
    NetworkPolicyRuleRaw, NetworkPolicyUpdate,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::handlers::progress::Bar;
use crate::utils;

/// One bundled policy that differs from what already exists in Thorium
///
/// Carries enough context to attribute the difference (rule/flag drift vs missing
/// group coverage) and, when updating is requested, to build the delta against the
/// distinct existing policies the toolbox definition overlaps.
pub struct PolicyMismatch {
    /// The policy name
    pub name: String,
    /// Target groups the policy is missing from in Thorium
    pub missing_groups: Vec<String>,
    /// The toolbox's chosen definition for this policy
    pub chosen: NetworkPolicyRequest,
    /// The distinct existing Thorium policies (by id) this overlaps
    pub existing: Vec<NetworkPolicy>,
}

/// A single existing Thorium policy to update toward the toolbox's definition
///
/// One bundled policy name can map to several distinct existing policies (the same
/// name in different groups is a different policy), so each gets its own plan.
pub struct PolicyUpdatePlan {
    /// The policy name
    pub name: String,
    /// The id of the existing policy this update targets
    pub id: Uuid,
    /// A toolbox target group the targeted policy lives in, for display
    pub group: String,
    /// The rule/flag fields being overwritten (empty for a groups-only add)
    pub drift: Vec<String>,
    /// The groups being added to the policy (additive only)
    pub add_groups: Vec<String>,
    /// The toolbox's chosen definition, used to rebuild the delta against the
    /// freshly-fetched policy state at apply time
    pub chosen: NetworkPolicyRequest,
}

/// Render one raw IP block in the canonical form Thorium stores it in
///
/// Thorium parses each CIDR on create and renders it back with its network length
/// always included (`10.0.0.1` is stored as `10.0.0.1/32`, and IPv6 text is
/// compressed/lowercased), so a hand-written block must be canonicalized the same
/// way before comparing, or it would never match the stored policy. A block that
/// fails to parse is returned unchanged so the server still reports the real error.
///
/// # Arguments
///
/// * `block` - The raw IP block to canonicalize
fn normalize_ip_block(block: &IpBlockRaw) -> IpBlockRaw {
    // parse the block exactly like the server does, which yields a single stored block
    let parsed = NetworkPolicyRule::default().raw_cidr(block.cidr.clone(), block.except.clone());
    // render the stored block back to its raw string form, or keep the input if unparsable
    match parsed {
        Ok(rule) => rule
            .allowed_ips
            .first()
            .map_or_else(|| block.clone(), IpBlockRaw::from),
        Err(_) => block.clone(),
    }
}

/// Canonicalize every IP block in one direction's rules
///
/// # Arguments
///
/// * `rules` - The ingress or egress rules to canonicalize in place
fn normalize_rules(rules: Option<&mut Vec<NetworkPolicyRuleRaw>>) {
    // a direction with no rules has nothing to canonicalize
    let Some(rules) = rules else {
        return;
    };
    // rewrite each rule's IP blocks into Thorium's stored form
    for rule in rules {
        rule.allowed_ips = rule.allowed_ips.iter().map(normalize_ip_block).collect();
    }
}

/// Normalize a policy request so semantically equal policies compare equal
///
/// Group order carries no meaning, so groups are sorted and de-duplicated; rule
/// order is preserved since rules are applied as written. CIDRs are rendered in
/// the canonical form Thorium stores them in (see [`normalize_ip_block`]).
///
/// # Arguments
///
/// * `policy` - The policy request to normalize
pub(super) fn normalize(mut policy: NetworkPolicyRequest) -> NetworkPolicyRequest {
    // sort and de-duplicate the groups since their order and repetition carry no meaning
    policy.groups.sort_unstable();
    policy.groups.dedup();
    // canonicalize the CIDRs in both directions so they match the stored form
    normalize_rules(policy.ingress.as_mut());
    normalize_rules(policy.egress.as_mut());
    policy
}

/// List the top-level fields that differ between two policy requests, ignoring the
/// named fields
///
/// Used when comparing a bundled policy against an existing instance policy to ignore
/// group membership: the toolbox targets specific groups while the instance policy may
/// legitimately span more. Group coverage is compared separately (gaps become
/// `missing_groups`, or `add_groups` under `--update-network-policy`), so only rule/flag
/// drift is reported here.
///
/// # Arguments
///
/// * `ours` - The bundled policy from the toolbox
/// * `theirs` - The existing policy in the target instance
/// * `exclude` - Top-level field names to drop from both sides before comparing
fn diff_fields_excluding(
    ours: &NetworkPolicyRequest,
    theirs: &NetworkPolicyRequest,
    exclude: &[&str],
) -> Vec<String> {
    // compare serialized forms field by field so new model fields are
    // covered automatically
    let (Ok(ours), Ok(theirs)) = (serde_json::to_value(ours), serde_json::to_value(theirs)) else {
        return vec!["<unserializable>".to_string()];
    };
    let (serde_json::Value::Object(mut ours), serde_json::Value::Object(mut theirs)) =
        (ours, theirs)
    else {
        return vec!["<unserializable>".to_string()];
    };
    // drop the excluded fields from both sides so they don't count as differences
    for field in exclude {
        ours.remove(*field);
        theirs.remove(*field);
    }
    // keep every top-level field whose value differs (or is absent) on the other side
    let mut fields: Vec<String> = ours
        .iter()
        .filter(|(key, value)| theirs.get(key.as_str()) != Some(*value))
        .map(|(key, _)| key.clone())
        .collect();
    // sort so the reported field list is deterministic
    fields.sort_unstable();
    fields
}

/// Whether a listing error means the caller simply can't see the group's policies
///
/// Non-admins get a 401/403 when listing a group they aren't a member of, and a group
/// that doesn't exist yet (it will be created by the import) returns 401/404. Neither
/// can hold a policy the caller could match against, so both are treated as "no
/// existing policies" instead of failing the whole import or diff.
///
/// # Arguments
///
/// * `err` - The error returned by the listing call
fn is_invisible_group(err: &Error) -> bool {
    matches!(
        err.status(),
        Some(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND)
    )
}

/// Fetch the network policies that already exist in the given groups, indexed by
/// `(group, name)`
///
/// Scoping the lookup to specific groups is what resolves cross-group name ambiguity:
/// within a single group a name maps to at most one policy, so this avoids the global
/// get-by-name that 400s when several distinct policies share a name across groups.
/// Mirrors how `network-policies describe --group` lists by group.
///
/// Each group is listed separately so a group the caller can't see (not a member, or
/// not created yet) contributes no existing policies rather than failing the lookup for
/// every other group (see [`is_invisible_group`]).
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups to scope the lookup to
pub(super) async fn fetch_existing_in_groups(
    thorium: &Thorium,
    groups: &[String],
) -> Result<HashMap<(String, String), NetworkPolicy>, Error> {
    // the existing policies found so far, keyed by (group, name)
    let mut existing = HashMap::new();
    for group in groups {
        // list (with full details) only the policies in this one group
        let opts = NetworkPolicyListOpts::default().groups(vec![group.clone()]);
        let mut cursor = match thorium.network_policies.list_details(&opts).await {
            Ok(cursor) => cursor,
            // a group we can't see holds no policies we could match, so skip it
            Err(err) if is_invisible_group(&err) => continue,
            Err(err) => {
                return Err(Error::new(format!(
                    "Failed to list network policies for group '{group}': {err}"
                )));
            }
        };
        loop {
            // index each policy under the queried group; a name is unique within a group,
            // so (group, name) identifies exactly one policy
            for policy in cursor.data.drain(..) {
                if policy.groups.iter().any(|member| member == group) {
                    existing.insert((group.clone(), policy.name.clone()), policy);
                }
            }
            // stop once every page of this group has been read
            if cursor.exhausted() {
                break;
            }
            // fetch the next page of this group's policies
            cursor.refill().await.map_err(|err| {
                Error::new(format!(
                    "Failed to list network policies for group '{group}': {err}"
                ))
            })?;
        }
    }
    Ok(existing)
}

/// Find the policy of a name in the first of the given groups that has one
///
/// A name is unique within a group but not across groups, so a policy a manifest or a snapshot
/// names is the one in the first of its groups that holds it.
///
/// # Arguments
///
/// * `existing` - The existing policies, indexed by `(group, name)`
/// * `groups` - The groups to look in, in order
/// * `name` - The name of the policy
pub(super) fn find_in_groups<'a>(
    existing: &'a HashMap<(String, String), NetworkPolicy>,
    groups: &[String],
    name: &str,
) -> Option<&'a NetworkPolicy> {
    // the first group that holds a policy of this name wins
    groups
        .iter()
        .find_map(|group| existing.get(&(group.clone(), name.to_owned())))
}

/// The outcome of categorizing one bundled policy against the existing instance state
///
/// `Mismatched` can't derive `PartialEq`/`Eq` because it carries
/// [`NetworkPolicy`] (no `Eq`), so tests match on it rather than comparing it.
#[derive(Debug)]
pub(super) enum PolicyClassification {
    /// The policy exists in no target group and will be created
    New,
    /// The policy already exists in every target group and matches
    Unchanged,
    /// The policy exists but differs (rule/flag drift) and/or is missing from some
    /// target groups; carries the missing groups and the distinct existing policies it
    /// overlaps for the warning/update path
    Mismatched {
        /// Target groups the policy is missing from in Thorium
        missing_groups: Vec<String>,
        /// The distinct existing policies (by id) this overlaps
        existing: Vec<NetworkPolicy>,
    },
}

/// Classify one bundled policy against the existing `(group, name)` index
///
/// A policy absent from every target group is new (safe to create). A policy present in
/// any target group is never created — create rejects a name already in any requested
/// group — so its rules/flags are compared (ignoring group membership) and any drift,
/// or partial coverage across the target groups, is reported as a mismatch.
///
/// # Arguments
///
/// * `policy` - The bundled policy (its `groups` are the target groups)
/// * `existing` - The existing policies indexed by `(group, name)`
pub(super) fn classify_policy(
    policy: &NetworkPolicyRequest,
    existing: &HashMap<(String, String), NetworkPolicy>,
) -> PolicyClassification {
    // split the target groups into those the policy already exists in and those it
    // doesn't, so we can tell "create" from "exists" from "partially exists"
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for group in &policy.groups {
        // file each target group by whether the policy name already exists there
        if existing.contains_key(&(group.clone(), policy.name.clone())) {
            present.push(group.clone());
        } else {
            missing.push(group.clone());
        }
    }
    // absent everywhere it would be created → new
    if present.is_empty() {
        return PolicyClassification::New;
    }
    // exists in at least one target group → never created; gather the distinct existing
    // policies it overlaps (by id) and the rule/flag drift against them
    let mut overlapped: Vec<NetworkPolicy> = Vec::new();
    let mut drift: Vec<String> = Vec::new();
    for group in &present {
        // look up the existing policy that holds this name in this group
        if let Some(existing_policy) = existing.get(&(group.clone(), policy.name.clone())) {
            // record each distinct existing policy once, since the same name across groups
            // can resolve to several independent policies
            if !overlapped.iter().any(|seen| seen.id == existing_policy.id) {
                overlapped.push(existing_policy.clone());
            }
            // surface the first rule/flag drift we find (group membership ignored)
            if drift.is_empty() {
                // compare against the stored form so both sides share one representation
                let diff = diff_fields_excluding(
                    policy,
                    &NetworkPolicyRequest::from(existing_policy),
                    &["groups"],
                );
                // keep this policy's drift only when it actually differs
                if !diff.is_empty() {
                    drift = diff;
                }
            }
        }
    }
    // sort the uncovered target groups (partial coverage) for deterministic output
    missing.sort_unstable();
    // matches everywhere it exists and covers every target group → nothing to do
    if drift.is_empty() && missing.is_empty() {
        return PolicyClassification::Unchanged;
    }
    PolicyClassification::Mismatched {
        missing_groups: missing,
        existing: overlapped,
    }
}

/// Build the per-policy update plans for one mismatch
///
/// A bundled name can overlap several distinct existing policies (the same name in
/// different groups), so one plan is produced per existing policy. Rule/flag drift
/// overwrites that policy; coverage gaps (target groups with no existing policy at all)
/// are folded into the first existing policy so the toolbox's coverage is satisfied
/// without removing anything.
///
/// # Arguments
///
/// * `mismatch` - The classified mismatch to turn into update plans
pub(super) fn build_updates(mismatch: &PolicyMismatch) -> Vec<PolicyUpdatePlan> {
    // sort the overlapped policies by id so the choice of "first" (which absorbs the
    // coverage gaps) is deterministic
    let mut existing = mismatch.existing.clone();
    existing.sort_by_key(|policy| policy.id);
    // the per-policy update plans produced for this mismatch
    let mut plans = Vec::new();
    for (idx, policy) in existing.iter().enumerate() {
        // recompute drift against this specific policy: distinct policies sharing a name
        // can differ from the toolbox in different ways
        let drift = diff_fields_excluding(
            &mismatch.chosen,
            &NetworkPolicyRequest::from(policy),
            &["groups"],
        );
        // only the truly-missing groups (no existing policy anywhere) need adding, and
        // they go to the first policy; groups already covered by a sibling policy are
        // left alone so we never create a duplicate name within a group
        let add_groups = if idx == 0 {
            mismatch.missing_groups.clone()
        } else {
            Vec::new()
        };
        // this policy already matches and needs no new coverage → nothing to do
        if drift.is_empty() && add_groups.is_empty() {
            continue;
        }
        // display a group the toolbox targets that this policy is actually in, so the
        // label points at the overlap rather than an unrelated group of the policy
        let group = mismatch
            .chosen
            .groups
            .iter()
            .find(|target| policy.groups.contains(target))
            .or_else(|| policy.groups.first())
            .cloned()
            .unwrap_or_default();
        plans.push(PolicyUpdatePlan {
            name: mismatch.name.clone(),
            id: policy.id,
            group,
            drift,
            add_groups,
            chosen: mismatch.chosen.clone(),
        });
    }
    plans
}

/// Build the delta to make an existing policy match the toolbox's chosen definition
///
/// `add_groups` is applied as-is (additive only). Rule/flag fields are touched only when
/// they appear in `drift`, so a groups-only add leaves rules untouched. Ingress/egress
/// follow the three-case semantics in [`rule_direction_delta`].
///
/// # Arguments
///
/// * `chosen` - The toolbox's chosen definition
/// * `drift` - The rule/flag fields that differ (from [`diff_fields_excluding`])
/// * `add_groups` - The groups to add to the policy
/// * `existing` - The current state of the policy being updated
fn build_delta(
    chosen: &NetworkPolicyRequest,
    drift: &[String],
    add_groups: &[String],
    existing: &NetworkPolicy,
) -> NetworkPolicyUpdate {
    // start from a groups-only delta; rule/flag fields are filled in only when they drifted
    let mut update = NetworkPolicyUpdate {
        add_groups: add_groups.to_vec(),
        ..NetworkPolicyUpdate::default()
    };
    // overwrite ingress only when it drifted, sourcing the removed rule UUIDs from this
    // exact policy's current state
    if drift.iter().any(|field| field == "ingress") {
        let (clear, deny_all, remove, add) =
            rule_direction_delta(chosen.ingress.as_ref(), existing.ingress.as_ref());
        update.clear_ingress = clear;
        update.deny_all_ingress = deny_all;
        update.remove_ingress = remove;
        update.add_ingress = add;
    }
    // egress is symmetric
    if drift.iter().any(|field| field == "egress") {
        let (clear, deny_all, remove, add) =
            rule_direction_delta(chosen.egress.as_ref(), existing.egress.as_ref());
        update.clear_egress = clear;
        update.deny_all_egress = deny_all;
        update.remove_egress = remove;
        update.add_egress = add;
    }
    // set the forced flag only when it differs
    if drift.iter().any(|field| field == "forced_policy") {
        update.forced_policy = Some(chosen.forced_policy);
    }
    // set the default flag only when it differs
    if drift.iter().any(|field| field == "default_policy") {
        update.default_policy = Some(chosen.default_policy);
    }
    update
}

/// Build the ingress/egress portion of a delta to make one direction's rules match a
/// target set of rules
///
/// Returns `(clear, deny_all, remove, add)` matching the `*_ingress`/`*_egress` fields of
/// [`NetworkPolicyUpdate`]. The three cases mirror the model's rule semantics: a `None`
/// target imposes no rules (allow-all) so the direction is cleared; an empty `Some`
/// denies all traffic, which must use the `deny_all` flag rather than a bare remove (a
/// bare remove would collapse the direction back to allow-all); a populated `Some` drops
/// the existing rules by id and adds the target rules.
///
/// # Arguments
///
/// * `target` - The rules the direction should end up with (the toolbox or prior state)
/// * `existing` - The direction's current rules, whose ids are removed
pub(crate) fn rule_direction_delta(
    target: Option<&Vec<NetworkPolicyRuleRaw>>,
    existing: Option<&Vec<NetworkPolicyRule>>,
) -> (bool, bool, Vec<Uuid>, Vec<NetworkPolicyRuleRaw>) {
    match target {
        // no rules → allow all → clear the direction
        None => (true, false, Vec::new(), Vec::new()),
        // empty rule set → deny all; use the flag, not an empty remove
        Some(rules) if rules.is_empty() => (false, true, Vec::new(), Vec::new()),
        // explicit rules → drop the existing rules by id and add the target rules
        Some(rules) => {
            // every existing rule is replaced, so collect all of their ids for removal
            let remove =
                existing.map_or_else(Vec::new, |rules| rules.iter().map(|rule| rule.id).collect());
            (false, false, remove, rules.clone())
        }
    }
}

/// Update one existing Thorium policy toward the toolbox's definition
///
/// The policy is fetched immediately before it is patched so the snapshot handed back is its true
/// pre-update state and the delta's removed rule UUIDs come from the current state.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `plan` - The planned update
/// * `progress` - The progress bar to log the action through, if there is one
/// * `snapshot` - Called with the policy as it is before the patch is sent
pub(super) async fn update_one(
    thorium: &Thorium,
    plan: &PolicyUpdatePlan,
    progress: Option<&Bar>,
    snapshot: impl FnOnce(&NetworkPolicy),
) -> Result<NetworkPolicy, Error> {
    // fetch the policy's current state right before mutating it
    let current = thorium
        .network_policies
        .get(&plan.name, Some(plan.id))
        .await
        .map_err(|err| {
            Error::new(format!(
                "Failed to fetch network policy '{}'{} before update: {err}",
                plan.name,
                utils::policy_suffix(utils::NO_GROUPS, Some(&plan.id))
            ))
        })?;
    // hand the pre-update snapshot over before the patch so a rollback can restore it
    snapshot(&current);
    // build the delta from the freshly-fetched state so removed rule UUIDs are current
    let delta = build_delta(&plan.chosen, &plan.drift, &plan.add_groups, &current);
    // log the action being taken, splitting overwrite from a groups-only add
    if let Some(progress) = progress {
        if !plan.drift.is_empty() {
            progress.info_anonymous(format!(
                "Updating network policy '{}'{} in Thorium to match the toolbox (differs: [{}])",
                plan.name,
                utils::policy_suffix(&[&plan.group], Some(&plan.id)),
                plan.drift.join(", ")
            ));
        }
        if !plan.add_groups.is_empty() {
            progress.info_anonymous(format!(
                "Adding group(s) [{}] to existing network policy '{}'{}",
                plan.add_groups.join(", "),
                plan.name,
                utils::policy_suffix(&[&plan.group], Some(&plan.id))
            ));
        }
    }
    // apply the patch targeting this exact policy by id
    thorium
        .network_policies
        .update(&plan.name, Some(plan.id), &delta)
        .await
        .map_err(|err| {
            Error::new(format!(
                "Failed to update network policy '{}'{}: {err}",
                plan.name,
                utils::policy_suffix(utils::NO_GROUPS, Some(&plan.id))
            ))
        })?;
    Ok(current)
}

/// Look up the id Thorium assigned to a policy this import just created
///
/// The create route returns no body, and the same name may already exist in other
/// groups, so the new policy is found by listing its own target groups (where the name
/// is unique). Returns `None` when the lookup fails or finds nothing; the caller still
/// has the name to fall back on.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `policy` - The policy request that was just created
async fn created_policy_id(thorium: &Thorium, policy: &NetworkPolicyRequest) -> Option<Uuid> {
    // list the policy's target groups, where its name is unique
    let existing = fetch_existing_in_groups(thorium, &policy.groups)
        .await
        .ok()?;
    // take the id from the first target group that holds the new policy
    policy
        .groups
        .iter()
        .find_map(|group| existing.get(&(group.clone(), policy.name.clone())))
        .map(|created| created.id)
}

/// Create one network policy and look up the id Thorium gave it
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `policy` - The policy to create
pub(super) async fn create_one(
    thorium: &Thorium,
    policy: &NetworkPolicyRequest,
) -> Result<Option<Uuid>, Error> {
    // create the policy in the instance; the request already carries its target groups
    if let Err(err) = thorium.network_policies.create(policy.clone()).await {
        // only admins may create network policies, so name that requirement when refused
        let hint = if matches!(
            err.status(),
            Some(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        ) {
            " (creating network policies requires a Thorium admin)"
        } else {
            ""
        };
        return Err(Error::new(format!(
            "Failed to create network policy '{}'{hint}: {err}",
            policy.name
        )));
    }
    // resolve the new policy's id so it can be told apart from same-named policies in other groups
    Ok(created_policy_id(thorium, policy).await)
}

/// Unit tests for the pure (no-I/O) policy logic: dedupe/conflict resolution,
/// classification against existing state, delta construction, and update planning
#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal policy with the given name, groups, and `default_policy` flag (the flag
    /// is just a cheap way to make two policies differ)
    ///
    /// # Arguments
    ///
    /// * `name` - The policy name
    /// * `groups` - The groups the policy targets
    /// * `default_policy` - Whether the policy is a default policy
    fn policy(name: &str, groups: &[&str], default_policy: bool) -> NetworkPolicyRequest {
        NetworkPolicyRequest {
            name: name.to_string(),
            groups: groups.iter().map(|group| (*group).to_string()).collect(),
            ingress: None,
            egress: None,
            forced_policy: false,
            default_policy,
        }
    }

    /// Build a stored `NetworkPolicy` for the existing-index and delta tests
    ///
    /// # Arguments
    ///
    /// * `name` - The policy name
    /// * `groups` - The groups the policy is in
    /// * `default_policy` - Whether the policy is a default policy
    /// * `id` - The policy's id
    fn existing_policy(
        name: &str,
        groups: &[&str],
        default_policy: bool,
        id: Uuid,
    ) -> NetworkPolicy {
        NetworkPolicy {
            name: name.to_string(),
            id,
            k8s_name: format!("{name}-{id}"),
            groups: groups.iter().map(|group| (*group).to_string()).collect(),
            created: chrono::DateTime::from_timestamp(0, 0).expect("valid epoch"),
            ingress: None,
            egress: None,
            forced_policy: false,
            default_policy,
            used_by: HashMap::new(),
        }
    }

    /// Build an existing-policy index keyed by `(group, name)` from `(group, policy)` pairs
    ///
    /// # Arguments
    ///
    /// * `entries` - The `(group, policy)` pairs to index
    fn index(entries: Vec<(&str, NetworkPolicy)>) -> HashMap<(String, String), NetworkPolicy> {
        entries
            .into_iter()
            .map(|(group, policy)| ((group.to_string(), policy.name.clone()), policy))
            .collect()
    }

    /// The policy of a name is read from the first group that holds one, even when another
    /// group holds a different policy of the same name
    #[test]
    fn find_in_groups_takes_the_first_group_that_has_the_name() {
        let first = existing_policy("allow-all", &["a"], false, Uuid::from_u128(1));
        let second = existing_policy("allow-all", &["b"], true, Uuid::from_u128(2));
        let existing = index(vec![("a", first), ("b", second)]);
        let groups = vec!["b".to_string(), "a".to_string()];
        let found = find_in_groups(&existing, &groups, "allow-all").expect("a policy");
        assert_eq!(found.id, Uuid::from_u128(2));
        let found = find_in_groups(&existing, &["a".to_string()], "allow-all").expect("a policy");
        assert_eq!(found.id, Uuid::from_u128(1));
    }

    /// A name no listed group holds is not found, whatever other groups hold
    #[test]
    fn find_in_groups_ignores_groups_outside_the_list() {
        let existing = index(vec![(
            "a",
            existing_policy("allow-all", &["a"], false, Uuid::from_u128(1)),
        )]);
        assert!(find_in_groups(&existing, &["b".to_string()], "allow-all").is_none());
        assert!(find_in_groups(&existing, &["a".to_string()], "other").is_none());
    }

    /// A policy absent from its target group is classified new (safe to create)
    #[test]
    fn classify_new_when_absent() {
        let existing = index(vec![]);
        let bundled = policy("allow-all", &["b"], false);
        assert!(matches!(
            classify_policy(&bundled, &existing),
            PolicyClassification::New
        ));
    }

    /// A policy with the same name in a *different* group is not a match — scoping by
    /// group is what disambiguates the otherwise-ambiguous name
    #[test]
    fn classify_new_when_same_name_other_group() {
        let existing = index(vec![(
            "a",
            existing_policy("allow-all", &["a"], false, Uuid::from_u128(1)),
        )]);
        let bundled = policy("allow-all", &["b"], false);
        assert!(matches!(
            classify_policy(&bundled, &existing),
            PolicyClassification::New
        ));
    }

    /// A policy present in its target group with matching rules is unchanged
    #[test]
    fn classify_unchanged_when_rules_match() {
        let existing = index(vec![(
            "a",
            existing_policy("allow-all", &["a"], false, Uuid::from_u128(1)),
        )]);
        let bundled = policy("allow-all", &["a"], false);
        assert!(matches!(
            classify_policy(&bundled, &existing),
            PolicyClassification::Unchanged
        ));
    }

    /// An instance policy that spans more groups than the bundle targets is still
    /// unchanged when the rules match (group membership is ignored in the comparison)
    #[test]
    fn classify_unchanged_ignores_group_superset() {
        let existing = index(vec![(
            "a",
            existing_policy("allow-all", &["a", "z"], false, Uuid::from_u128(1)),
        )]);
        let bundled = policy("allow-all", &["a"], false);
        assert!(matches!(
            classify_policy(&bundled, &existing),
            PolicyClassification::Unchanged
        ));
    }

    /// Rule/flag drift against an existing policy is a mismatch carrying the drift fields
    /// and no missing groups
    #[test]
    fn classify_mismatched_on_rule_drift() {
        // differ only on default_policy
        let existing = index(vec![(
            "a",
            existing_policy("allow-all", &["a"], true, Uuid::from_u128(1)),
        )]);
        let bundled = policy("allow-all", &["a"], false);
        match classify_policy(&bundled, &existing) {
            PolicyClassification::Mismatched {
                missing_groups,
                existing,
            } => {
                assert!(missing_groups.is_empty());
                assert_eq!(existing.len(), 1);
            }
            other => panic!("expected mismatched, got {other:?}"),
        }
    }

    /// A policy that targets multiple groups but exists in only some is a mismatch that
    /// names the missing groups, with no rule/flag drift
    #[test]
    fn classify_mismatched_on_partial_coverage() {
        let existing = index(vec![(
            "a",
            existing_policy("allow-all", &["a"], false, Uuid::from_u128(1)),
        )]);
        let bundled = policy("allow-all", &["a", "b"], false);
        match classify_policy(&bundled, &existing) {
            PolicyClassification::Mismatched { missing_groups, .. } => {
                assert_eq!(missing_groups, vec!["b".to_string()]);
            }
            other => panic!("expected mismatched, got {other:?}"),
        }
    }

    /// Rule drift and a coverage gap can occur together; both are reported
    #[test]
    fn classify_mismatched_drift_and_coverage() {
        let existing = index(vec![(
            "a",
            existing_policy("allow-all", &["a"], true, Uuid::from_u128(1)),
        )]);
        let bundled = policy("allow-all", &["a", "b"], false);
        match classify_policy(&bundled, &existing) {
            PolicyClassification::Mismatched { missing_groups, .. } => {
                assert_eq!(missing_groups, vec!["b".to_string()]);
            }
            other => panic!("expected mismatched, got {other:?}"),
        }
    }

    /// `None` rules mean "allow all", so the direction is cleared
    #[test]
    fn rule_direction_delta_none_clears() {
        let (clear, deny_all, remove, add) = rule_direction_delta(None, None);
        assert!(clear);
        assert!(!deny_all);
        assert!(remove.is_empty());
        assert!(add.is_empty());
    }

    /// An empty rule set means "deny all", which must use the flag, not a bare remove
    #[test]
    fn rule_direction_delta_empty_denies_all() {
        let target: Vec<NetworkPolicyRuleRaw> = Vec::new();
        let (clear, deny_all, remove, add) = rule_direction_delta(Some(&target), None);
        assert!(!clear);
        assert!(deny_all);
        assert!(remove.is_empty());
        assert!(add.is_empty());
    }

    /// Explicit rules drop the existing rules by id and add the target rules
    #[test]
    fn rule_direction_delta_rules_remove_and_add() {
        let rule_id = Uuid::from_u128(7);
        let existing_rules = vec![NetworkPolicyRule {
            id: rule_id,
            ..NetworkPolicyRule::default()
        }];
        let target = vec![NetworkPolicyRuleRaw {
            allowed_local: true,
            ..NetworkPolicyRuleRaw::default()
        }];
        let (clear, deny_all, remove, add) =
            rule_direction_delta(Some(&target), Some(&existing_rules));
        assert!(!clear);
        assert!(!deny_all);
        assert_eq!(remove, vec![rule_id]);
        assert_eq!(add.len(), 1);
        assert!(add[0].allowed_local);
    }

    /// A groups-only update adds the groups and never touches rules or flags, and never
    /// removes groups
    #[test]
    fn build_delta_groups_only() {
        let chosen = policy("p", &["a", "b"], false);
        let existing = existing_policy("p", &["a"], false, Uuid::from_u128(1));
        let delta = build_delta(&chosen, &[], &["b".to_string()], &existing);
        assert_eq!(delta.add_groups, vec!["b".to_string()]);
        assert!(delta.remove_groups.is_empty());
        assert!(delta.remove_ingress.is_empty());
        assert!(delta.add_ingress.is_empty());
        assert!(!delta.clear_ingress);
        assert!(!delta.deny_all_ingress);
        assert!(delta.forced_policy.is_none());
        assert!(delta.default_policy.is_none());
    }

    /// A rule-drift update removes the existing ingress by id, adds the toolbox's ingress,
    /// and toggles only the flags that drifted, never removing groups
    #[test]
    fn build_delta_rule_drift() {
        let rule_id = Uuid::from_u128(9);
        let mut existing = existing_policy("p", &["a"], false, Uuid::from_u128(1));
        existing.ingress = Some(vec![NetworkPolicyRule {
            id: rule_id,
            ..NetworkPolicyRule::default()
        }]);
        let mut chosen = policy("p", &["a"], true);
        chosen.ingress = Some(vec![NetworkPolicyRuleRaw {
            allowed_local: true,
            ..NetworkPolicyRuleRaw::default()
        }]);
        let drift = vec!["ingress".to_string(), "default_policy".to_string()];
        let delta = build_delta(&chosen, &drift, &[], &existing);
        assert_eq!(delta.remove_ingress, vec![rule_id]);
        assert_eq!(delta.add_ingress.len(), 1);
        assert!(delta.add_ingress[0].allowed_local);
        assert_eq!(delta.default_policy, Some(true));
        assert!(delta.forced_policy.is_none());
        assert!(delta.remove_groups.is_empty());
    }

    /// `None` toolbox ingress clears the direction (allow-all)
    #[test]
    fn build_delta_clears_ingress_when_none() {
        let mut existing = existing_policy("p", &["a"], false, Uuid::from_u128(1));
        existing.ingress = Some(vec![NetworkPolicyRule {
            id: Uuid::from_u128(9),
            ..NetworkPolicyRule::default()
        }]);
        // chosen.ingress is None (allow all)
        let chosen = policy("p", &["a"], false);
        let delta = build_delta(&chosen, &["ingress".to_string()], &[], &existing);
        assert!(delta.clear_ingress);
        assert!(!delta.deny_all_ingress);
        assert!(delta.add_ingress.is_empty());
    }

    /// An empty-`Some` toolbox ingress denies all via the flag, not an empty remove
    #[test]
    fn build_delta_deny_all_when_empty_some() {
        let existing = existing_policy("p", &["a"], false, Uuid::from_u128(1));
        let mut chosen = policy("p", &["a"], false);
        chosen.ingress = Some(Vec::new());
        let delta = build_delta(&chosen, &["ingress".to_string()], &[], &existing);
        assert!(delta.deny_all_ingress);
        assert!(!delta.clear_ingress);
        assert!(delta.add_ingress.is_empty());
    }

    /// One bundled name overlapping two distinct existing policies updates only the one
    /// that drifts, leaving the matching one untouched
    #[test]
    fn build_updates_only_targets_drifting_policy() {
        // policy "p" matches in group a (id 1) but differs in group b (id 2)
        let matching = existing_policy("p", &["a"], false, Uuid::from_u128(1));
        let drifting = existing_policy("p", &["b"], true, Uuid::from_u128(2));
        let mismatch = PolicyMismatch {
            name: "p".to_string(),
            missing_groups: Vec::new(),
            chosen: policy("p", &["a", "b"], false),
            existing: vec![matching, drifting],
        };
        let updates = build_updates(&mismatch);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id, Uuid::from_u128(2));
        assert!(
            updates[0]
                .drift
                .iter()
                .any(|field| field == "default_policy")
        );
    }

    /// A coverage gap is folded into the first existing policy as an additive group add
    #[test]
    fn build_updates_adds_missing_coverage() {
        let existing = existing_policy("p", &["a"], false, Uuid::from_u128(1));
        let mismatch = PolicyMismatch {
            name: "p".to_string(),
            missing_groups: vec!["b".to_string()],
            chosen: policy("p", &["a", "b"], false),
            existing: vec![existing],
        };
        let updates = build_updates(&mismatch);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].add_groups, vec!["b".to_string()]);
        assert!(updates[0].drift.is_empty());
    }

    /// Build a policy whose egress allows the given `(cidr, except)` blocks
    ///
    /// # Arguments
    ///
    /// * `blocks` - The raw `(cidr, except)` pairs to allow
    fn egress_policy(blocks: &[(&str, Option<Vec<&str>>)]) -> NetworkPolicyRequest {
        // start from a minimal policy in group `a`
        let mut request = policy("p", &["a"], false);
        // turn each pair into a raw IP block in a single egress rule
        let allowed_ips = blocks
            .iter()
            .map(|(cidr, except)| IpBlockRaw {
                cidr: (*cidr).to_string(),
                except: except
                    .as_ref()
                    .map(|except| except.iter().map(|cidr| (*cidr).to_string()).collect()),
            })
            .collect();
        request.egress = Some(vec![NetworkPolicyRuleRaw {
            allowed_ips,
            ..NetworkPolicyRuleRaw::default()
        }]);
        request
    }

    /// Host addresses gain their full prefix length and IPv6 text is canonicalized,
    /// matching the form Thorium stores
    #[test]
    fn normalize_canonicalizes_cidrs() {
        let normalized = normalize(egress_policy(&[
            ("10.0.0.1", None),
            ("10.0.0.0/8", Some(vec!["10.1.0.0/16", "10.2.3.4"])),
            ("2001:DB8:0:0:0:0:0:1", None),
            ("not-a-cidr", None),
        ]));
        let blocks = &normalized.egress.as_ref().expect("egress kept")[0].allowed_ips;
        // a bare IPv4 host becomes a /32
        assert_eq!(blocks[0].cidr, "10.0.0.1/32");
        // an already-full network is unchanged, and its except hosts gain /32
        assert_eq!(blocks[1].cidr, "10.0.0.0/8");
        assert_eq!(
            blocks[1].except,
            Some(vec!["10.1.0.0/16".to_string(), "10.2.3.4/32".to_string()])
        );
        // an uncompressed uppercase IPv6 host is compressed, lowercased, and becomes a /128
        assert_eq!(blocks[2].cidr, "2001:db8::1/128");
        // an unparsable block is left for the server to reject
        assert_eq!(blocks[3].cidr, "not-a-cidr");
    }

    /// A hand-written host CIDR matches the stored `/32` form, so it reports no drift
    #[test]
    fn classify_unchanged_for_equivalent_cidrs() {
        // the stored policy carries the full-length form Thorium renders
        let mut stored = existing_policy("p", &["a"], false, Uuid::from_u128(1));
        stored.egress = Some(vec![
            NetworkPolicyRule::default()
                .raw_cidr("8.8.8.8/32".to_string(), None)
                .expect("valid cidr"),
        ]);
        let existing = index(vec![("a", stored)]);
        // the bundled policy is written with a bare host address
        let bundled = normalize(egress_policy(&[("8.8.8.8", None)]));
        assert!(matches!(
            classify_policy(&bundled, &existing),
            PolicyClassification::Unchanged
        ));
    }

    /// Repeated groups within one policy collapse to a single entry
    #[test]
    fn normalize_dedups_groups() {
        let normalized = normalize(policy("p", &["b", "a", "b"], false));
        assert_eq!(normalized.groups, vec!["a".to_string(), "b".to_string()]);
    }

    /// The update label names a toolbox target group the policy is in, not an unrelated one
    #[test]
    fn build_updates_labels_target_group() {
        // the existing policy spans `z` (unrelated) and `b` (targeted by the toolbox)
        let existing = existing_policy("p", &["z", "b"], true, Uuid::from_u128(1));
        let mismatch = PolicyMismatch {
            name: "p".to_string(),
            missing_groups: Vec::new(),
            chosen: policy("p", &["b"], false),
            existing: vec![existing],
        };
        let updates = build_updates(&mismatch);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].group, "b");
    }
}
