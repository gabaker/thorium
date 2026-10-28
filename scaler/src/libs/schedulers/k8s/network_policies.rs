use futures::stream::{self, StreamExt};
use hashbrown::HashMap;
use k8s_openapi::api::networking::v1::{
    NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicySpec,
};
use kube::api::{Api, DeleteParams, ListParams, ObjectList, PostParams};
use serde_json::json;
use std::collections::HashSet;
use thorium::{
    conf::BaseNetworkPolicy,
    models::{NetworkPolicyRule, ToolIdsByName},
};
use uuid::Uuid;

use crate::{
    libs::{
        Cache,
        cache::{NO_TOOLS, ToolIdsByGroup},
        helpers,
    },
    raw_entry_vec_extend, same,
};

/// A base network policy from the Thorium config with its rules converted
#[derive(Debug, Clone)]
pub struct BasePolicy {
    /// The base policy's name
    pub name: String,
    /// The base policy's ingress rules, if it restricts ingress
    pub ingress: Option<Vec<NetworkPolicyRule>>,
    /// The base policy's egress rules, if it restricts egress
    pub egress: Option<Vec<NetworkPolicyRule>>,
}

impl TryFrom<&BaseNetworkPolicy> for BasePolicy {
    type Error = thorium::Error;

    /// Convert the raw rules of a base network policy from the Thorium config
    ///
    /// # Arguments
    ///
    /// * `base_policy` - The base network policy to convert
    fn try_from(base_policy: &BaseNetworkPolicy) -> Result<Self, Self::Error> {
        // convert a list of raw rules
        let convert = |rules: &Option<Vec<thorium::models::NetworkPolicyRuleRaw>>| {
            rules
                .as_ref()
                .map(|rules| {
                    rules
                        .iter()
                        .cloned()
                        .map(NetworkPolicyRule::try_from)
                        .collect::<Result<Vec<NetworkPolicyRule>, thorium::Error>>()
                })
                .transpose()
        };
        Ok(BasePolicy {
            name: base_policy.name.clone(),
            ingress: convert(&base_policy.ingress)?,
            egress: convert(&base_policy.egress)?,
        })
    }
}

/// Overlay Thorium network policy rules on a K8s network policy for one namespace
///
/// The policy types follow which rule lists are set, and tool-name entries select the tools
/// `tools` resolves them to.
///
/// # Arguments
///
/// * `network_policy` - The K8s network policy to overlay the rules on
/// * `ingress` - The ingress rules, if the policy restricts ingress
/// * `egress` - The egress rules, if the policy restricts egress
/// * `tools` - The ids of the tools in the namespace's group holding each named tool name
fn overlay_rules(
    network_policy: &mut NetworkPolicy,
    ingress: Option<&Vec<NetworkPolicyRule>>,
    egress: Option<&Vec<NetworkPolicyRule>>,
    tools: &ToolIdsByName,
) {
    // get a mutable reference to the spec
    let netpol_spec = network_policy
        .spec
        .get_or_insert(NetworkPolicySpec::default());
    // define the policy types we're applying to (ingress/egress)
    // based on the presence/absence of rules
    netpol_spec.policy_types = match (ingress, egress) {
        (None, None) => None,
        (None, Some(_)) => Some(vec!["Egress".to_string()]),
        (Some(_), None) => Some(vec!["Ingress".to_string()]),
        (Some(_), Some(_)) => Some(vec!["Ingress".to_string(), "Egress".to_string()]),
    };
    // convert each rule to its K8s form
    netpol_spec.ingress = ingress.map(|rules| {
        rules
            .iter()
            .map(|rule| rule.clone().to_k8s_ingress(tools))
            .collect::<Vec<NetworkPolicyIngressRule>>()
    });
    netpol_spec.egress = egress.map(|rules| {
        rules
            .iter()
            .map(|rule| rule.clone().to_k8s_egress(tools))
            .collect::<Vec<NetworkPolicyEgressRule>>()
    });
}

/// Wrapper for network policies api routes in k8s
pub struct NetworkPolicies {
    /// Client to use for creating namespaced clients
    client: kube::Client,
    /// A cache of information related to network policies that needs to be
    /// retained between runs
    pub cache: NetworkPolicyCache,
}

impl NetworkPolicies {
    /// Build new wrapper for k8s functions regarding network policies
    ///
    /// # Arguments
    ///
    /// * `client` - Kubernetes client
    /// * `conf` - Thorium Config
    pub fn new(client: &kube::Client, conf: &thorium::Conf) -> Result<Self, thorium::Error> {
        // get client for creating namespaced clients with
        let client = client.clone();
        // attempt to convert the rules of each base policy defined in the config
        let base_policies = conf.thorium.base_network_policies
            .iter()
            .map(BasePolicy::try_from)
            .collect::<Result<Vec<BasePolicy>, thorium::Error>>()
            .map_err(|err| {
                thorium::Error::new(format!("One or more of the base network policies in the Thorium config is invalid! {err}"))
            })?;
        let cache = NetworkPolicyCache::default().base_policies(base_policies);
        Ok(Self { client, cache })
    }

    /// List [`NetworkPolicy`]'s in a namespace in k8s
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace to list network policies from
    pub async fn list(&self, ns: &str) -> Result<ObjectList<NetworkPolicy>, kube::Error> {
        // get a namespaced client
        let api: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), ns);
        // use default list params
        let list_params = ListParams::default();
        // list the network policies for this namespace
        api.list(&list_params).await
    }

    /// Generate a network policy spec from a Thorium network policy for each of the
    /// given namespaces
    ///
    /// Returns a list of tuples containing the namespace and the corresponding
    /// spec. Each namespace's spec selects the tools its group's tool ids resolve the policy's
    /// tool-name entries to.
    ///
    /// # Arguments
    ///
    /// * `thorium_policy` - The Thorium network policy to generate the spec from
    /// * `namespaces` - The namespace to generate specs for
    /// * `tool_ids` - The ids of the tools holding each named tool name by group
    pub fn generate<I, T>(
        thorium_policy: &thorium::models::NetworkPolicy,
        namespaces: I,
        tool_ids: &ToolIdsByGroup,
    ) -> Vec<(String, NetworkPolicy)>
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        // build network policy spec(s)
        let raw = json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": {
                // refrain from including a namespace;
                // we'll copy the spec and add a namespace for each group the policy is in
                //"namespace":
                "name": &thorium_policy.k8s_name,
                "labels": {
                    "thorium": "true",
                }
            },
            "spec": {
                "podSelector": {
                    "matchLabels": {
                        // match thorium-spawned pods that are labeled with this policy
                        "thorium": "true",
                        &thorium_policy.k8s_name: "true"
                    }
                },
            }
        });
        // cast this json into a k8's network policy;
        // unwrap the result because we're sure that it's valid
        let network_policy: NetworkPolicy = serde_json::from_value(raw).unwrap();
        // return a policy spec for each given namespace
        namespaces
            .into_iter()
            .map(|ns| {
                let ns = ns.into();
                let mut policy_spec = network_policy.clone();
                policy_spec.metadata.namespace = Some(ns.clone());
                // overlay the Thorium network policy rules, with tool names resolved in this
                // namespace's group, on top of the the K8's network policy
                let tools = tool_ids.get(&ns).unwrap_or(&NO_TOOLS);
                overlay_rules(
                    &mut policy_spec,
                    thorium_policy.ingress.as_ref(),
                    thorium_policy.egress.as_ref(),
                    tools,
                );
                (ns, policy_spec)
            })
            .collect()
    }

    /// Generate a policy spec for one namespace from a base network policy
    ///
    /// # Arguments
    ///
    /// * `base_policy` - The base policy to generate the spec from
    /// * `tools` - The ids of the tools in the namespace's group holding each named tool name
    fn generate_base_policy(base_policy: &BasePolicy, tools: &ToolIdsByName) -> NetworkPolicy {
        // build network policy spec(s)
        let raw = json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": {
                // refrain from including a namespace;
                // we'll copy the spec and add a namespace for each group the base policy needs to be in
                //"namespace":
                "name": &base_policy.name,
                "labels": {
                    "thorium": "true",
                    "base_policy": "true"
                }
            },
            "spec": {
                "podSelector": {
                    "matchLabels": {
                        // match all thorium-spawned pods
                        "thorium": "true",
                    }
                },
            }
        });
        // cast this json into a k8's network policy;
        // unwrap the result because we're sure that it's valid
        let mut network_policy: NetworkPolicy = serde_json::from_value(raw).unwrap();
        // overlay the base policy's rules on top of the the K8's network policy
        overlay_rules(
            &mut network_policy,
            base_policy.ingress.as_ref(),
            base_policy.egress.as_ref(),
            tools,
        );
        // return our base K8's network policy spec
        network_policy
    }

    /// Deploys network policies within a specific namespace
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace to deploy the network policies to
    /// * `network_policies` - The network policies to deploy
    /// * `errors` - A list of errors and the underlying policy that caused that error
    pub async fn deploy(
        &self,
        ns: &str,
        network_policies: Vec<(Uuid, NetworkPolicy)>,
        errors: &mut Vec<(Uuid, String, kube::Error)>,
    ) {
        // get a namespaced client for our network policies' target namespace
        let api: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), ns);
        // set the default params
        let params = PostParams::default();
        // deploy network policies 5 at a time
        let deploys = helpers::assert_send_stream(
            stream::iter(network_policies.iter())
                .map(|(_, netpol)| {
                    // get references to minimize cloning
                    let api_ref = &api;
                    let params_ref = &params;
                    async move { api_ref.create(params_ref, netpol).await }
                })
                .buffered(5),
        )
        .collect::<Vec<Result<_, _>>>()
        .await;
        // crawl our network policies and determine if any failures occurred
        for ((id, policy), deploy) in network_policies.into_iter().zip(deploys) {
            // check if our deployment failed
            if let Err(error) = deploy {
                // this attempt failed so track this error
                errors.push((id, policy.metadata.name.unwrap_or_default(), error));
            }
        }
    }

    /// Deploy base network policies to the given namespace
    ///
    /// Returns a list of errors and the names of policies the errors arose from
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace to deploy the base network policies to
    /// * `base_policy_specs` - The specs of base policies to deploy
    pub async fn deploy_base<'a, I>(
        &'a self,
        ns: &str,
        base_policy_specs: I,
    ) -> Vec<(String, kube::Error)>
    where
        I: IntoIterator<Item = &'a NetworkPolicy> + Send,
        <I as IntoIterator>::IntoIter: std::marker::Send,
    {
        // get a namespaced client for our base network policies' target namespace
        let api: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), ns);
        // set the default params
        let params = PostParams::default();
        // deploy base network policies 5 at a time
        helpers::assert_send_stream(
            stream::iter(base_policy_specs)
                .map(|netpol| {
                    // get references to minimize cloning
                    let api_ref = &api;
                    let params_ref = &params;
                    async move { (netpol, api_ref.create(params_ref, netpol).await) }
                })
                .buffered(5),
        )
        .collect::<Vec<(_, Result<_, _>)>>()
        .await
        .into_iter()
        // return the names of base policies that failed to deploy and their errors
        .filter_map(|(policy, deploy)| {
            deploy
                .err()
                .map(|err| (policy.metadata.name.clone().unwrap_or_default(), err))
        })
        .collect()
    }

    /// Delete network policies from K8's with the given names at the given namespace
    ///
    /// Returns a list of policies that had an error when deleting and their errors
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace the policy is in
    /// * `policy_names` - The names of the policies to delete
    pub async fn delete_many(
        &self,
        ns: &str,
        policy_names: Vec<String>,
    ) -> Vec<(String, kube::Error)> {
        // get a namespaced client for our network policies' target namespace
        let api: Api<NetworkPolicy> = Api::namespaced(self.client.clone(), ns);
        // build delete params
        let params = DeleteParams::default().grace_period(0);
        // delete the policies 5 at a time
        let deletes = helpers::assert_send_stream(
            stream::iter(policy_names.iter())
                .map(|policy_name| {
                    // get references to minimize cloning
                    let api_ref = &api;
                    let params_ref = &params;
                    async move { api_ref.delete(policy_name, params_ref).await }
                })
                .buffered(5),
        )
        .collect::<Vec<Result<_, _>>>()
        .await;
        // collect errors and the policy names connected to them
        policy_names
            .into_iter()
            .zip(deletes)
            .filter_map(|(policy_name, delete)| delete.err().map(|err| (policy_name, err)))
            .collect()
    }
}

/// A cache of information related to network policies that needs to be
/// retained between runs
#[derive(Default)]
pub struct NetworkPolicyCache {
    /// The base network policies defined in the Thorium config, with their rules converted
    pub base_policies: Vec<BasePolicy>,
    /// The tool ids that tool-name entries resolved to when every policy was last reconciled
    /// with K8s, or `None` before the first reconcile
    pub reconciled_tool_ids: Option<ToolIdsByGroup>,
    /// The set of namespaces that base network policies have already been
    /// successfully deployed to
    pub namespaces_with_base_policies: HashSet<String>,
    /// ID's of policies that still need to be added
    pub policies_to_add: Vec<Uuid>,
    /// K8's names of policies that still need to be removed,
    /// mapped by group/namespace
    pub policies_to_remove: HashMap<String, Vec<String>>,
}

impl NetworkPolicyCache {
    /// Add base network policies to the cache
    ///
    /// # Arguments
    ///
    /// * `base_policies` - The base network policies to add
    fn base_policies(mut self, base_policies: Vec<BasePolicy>) -> Self {
        self.base_policies = base_policies;
        self
    }

    /// Check whether the tools that tool-name entries resolve to changed since every policy was
    /// last reconciled with K8s
    ///
    /// # Arguments
    ///
    /// * `tool_ids` - The tool ids tool-name entries resolve to now
    #[must_use]
    pub fn tools_changed(&self, tool_ids: &ToolIdsByGroup) -> bool {
        self.reconciled_tool_ids.as_ref() != Some(tool_ids)
    }

    /// Generate the specs of every base network policy for one namespace
    ///
    /// # Arguments
    ///
    /// * `tools` - The ids of the tools in the namespace's group holding each named tool name
    #[must_use]
    pub fn base_policy_specs(&self, tools: &ToolIdsByName) -> Vec<NetworkPolicy> {
        self.base_policies
            .iter()
            .map(|base_policy| NetworkPolicies::generate_base_policy(base_policy, tools))
            .collect()
    }

    /// Update the network policy cache with the contents of a [`Cache`] of Thorium data
    ///
    /// The reasoning here is there is certain info that can only be computed
    /// within the context of [`Cache::reload_data`] using the data from the
    /// previous (stale) cache (specifically, any policies that have been added,
    /// removed, or updated since last cache reload); we also retain info on
    /// policies that failed to be added/removed last time in this `NetworkPolicyCache`,
    /// so we need to combine the info from [`Cache`] and `NetworkPolicyCache` here.
    ///
    /// # Arguments
    ///
    /// * `thorium_cache` - A cache of info retrieved from Thorium
    pub fn update(&mut self, thorium_cache: &Cache) {
        // add any policies that have been added since last cache reload
        self.policies_to_add
            .extend(&thorium_cache.network_policies.policies_added);
        // add any policies that have been removed since last cache reload (in all of their namespaces)
        for (ns, policy_names) in &thorium_cache.network_policies.policies_removed {
            raw_entry_vec_extend!(self.policies_to_remove, ns, policy_names.clone());
        }
    }
}

/// Get a policy's spec with empty rule lists removed
///
/// K8s stores an empty ingress or egress list as no list, which means the same, so specs are
/// compared in this form.
///
/// # Arguments
///
/// * `policy` - The policy to get the spec of
fn comparable_spec(policy: &NetworkPolicy) -> Option<NetworkPolicySpec> {
    policy.spec.clone().map(|mut spec| {
        // drop the rule lists that hold no rules
        spec.ingress = spec.ingress.filter(|rules| !rules.is_empty());
        spec.egress = spec.egress.filter(|rules| !rules.is_empty());
        spec
    })
}

/// Check if the policy in K8's is equal to the cached policy
///
/// Note that the cached policy does not yet have a namespace, so checks that
/// the K8's policy is in the proper namespace should occur outside of this
/// function; there's also a lot of metadata in the K8's policy that the cached one
/// will not have (creation date, etc.) that we don't compare
///
/// # Arguments
///
/// * `k8s_policy` - The policy retrieved from K8's
/// * `cached_policy` - The policy from the Thorium cache
pub(super) fn policies_equal(k8s_policy: &NetworkPolicy, cached_policy: &NetworkPolicy) -> bool {
    same!(k8s_policy.metadata.name, cached_policy.metadata.name);
    same!(comparable_spec(k8s_policy), comparable_spec(cached_policy));
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use thorium::models::{NO_TOOL_LABEL_VALUE, NetworkPolicyRuleRaw, TOOL_LABEL};

    /// Get the tool ids a rendered policy's ingress peers select
    ///
    /// # Arguments
    ///
    /// * `policy` - The rendered K8s network policy
    fn selected_tools(policy: &NetworkPolicy) -> Vec<String> {
        policy
            .spec
            .clone()
            .unwrap()
            .ingress
            .unwrap()
            .into_iter()
            .flat_map(|rule| rule.from.unwrap_or_default())
            .filter_map(|peer| peer.pod_selector?.match_labels?.remove(TOOL_LABEL))
            .collect()
    }

    /// Build tool ids holding one name in each of the given groups
    ///
    /// # Arguments
    ///
    /// * `name` - The name the tools hold
    /// * `groups` - The groups and the tool holding the name in each
    fn tool_ids(name: &str, groups: &[(&str, Uuid)]) -> ToolIdsByGroup {
        groups
            .iter()
            .map(|(group, id)| {
                let tools: ToolIdsByName = [(name.to_owned(), [*id].into_iter().collect())]
                    .into_iter()
                    .collect();
                ((*group).to_owned(), tools)
            })
            .collect()
    }

    /// A policy's spec in each namespace selects the tool holding a named name in that
    /// namespace's group, and the spec changes when another tool takes the name, so a reconcile
    /// replaces the policy in K8s
    #[test]
    fn policy_specs_follow_the_tool_holding_a_name() {
        // a policy in two groups naming one tool
        let policy: thorium::models::NetworkPolicy = serde_json::from_value(json!({
            "name": "fenced",
            "id": Uuid::new_v4(),
            "k8s_name": "fenced-1234",
            "groups": ["ceres", "vesta"],
            "created": chrono::Utc::now(),
            "forced_policy": false,
            "default_policy": false,
            "used_by": {},
            "ingress": [{"id": Uuid::new_v4(), "allowed_tools": ["ghidra-cli"]}],
        }))
        .unwrap();
        let (first, second, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let ids = tool_ids("ghidra-cli", &[("ceres", first), ("vesta", other)]);
        let specs = NetworkPolicies::generate(&policy, ["ceres", "vesta"], &ids);
        // each namespace selects its own group's tool
        assert_eq!(specs[0].0, "ceres");
        assert_eq!(selected_tools(&specs[0].1), [first.to_string()]);
        assert_eq!(specs[1].0, "vesta");
        assert_eq!(selected_tools(&specs[1].1), [other.to_string()]);
        // a group where no tool holds the name selects no worker
        let specs = NetworkPolicies::generate(&policy, ["juno"], &ids);
        assert_eq!(selected_tools(&specs[0].1), [NO_TOOL_LABEL_VALUE]);
        // another tool taking the name changes the spec
        let renamed = tool_ids("ghidra-cli", &[("ceres", second), ("vesta", other)]);
        let before = NetworkPolicies::generate(&policy, ["ceres"], &ids);
        let after = NetworkPolicies::generate(&policy, ["ceres"], &renamed);
        assert!(!policies_equal(&before[0].1, &after[0].1));
        assert_eq!(selected_tools(&after[0].1), [second.to_string()]);
    }

    /// A base policy's tool-name entries resolve in each namespace's group like a policy's
    #[test]
    fn base_policy_specs_resolve_tool_names_per_namespace() {
        // a base policy naming a tool, converted like the scaler does at startup
        let base = BaseNetworkPolicy {
            name: "base".to_owned(),
            ingress: Some(vec![NetworkPolicyRuleRaw::default().tool("ghidra-cli")]),
            egress: None,
        };
        let cache =
            NetworkPolicyCache::default().base_policies(vec![BasePolicy::try_from(&base).unwrap()]);
        let tool = Uuid::new_v4();
        let ids = tool_ids("ghidra-cli", &[("ceres", tool)]);
        // the base policy selects the group's tool in its namespace and nothing elsewhere
        let ceres = cache.base_policy_specs(&ids["ceres"]);
        assert_eq!(ceres[0].metadata.name.as_deref(), Some("base"));
        assert_eq!(selected_tools(&ceres[0]), [tool.to_string()]);
        let elsewhere = cache.base_policy_specs(&NO_TOOLS);
        assert_eq!(selected_tools(&elsewhere[0]), [NO_TOOL_LABEL_VALUE]);
    }

    /// A policy whose rendered spec holds an empty rule list equals the K8s copy, which drops
    /// the empty list, so a reconcile doesn't replace it; a real difference still counts
    #[test]
    fn empty_rule_lists_equal_their_k8s_copy() {
        // a policy that allows no ingress and some egress, rendered for one namespace
        let policy: thorium::models::NetworkPolicy = serde_json::from_value(json!({
            "name": "mock-egress",
            "id": Uuid::new_v4(),
            "k8s_name": "mock-egress-1234",
            "groups": ["ceres"],
            "created": chrono::Utc::now(),
            "forced_policy": false,
            "default_policy": false,
            "used_by": {},
            "ingress": [],
            "egress": [{"id": Uuid::new_v4(), "allowed_all": true}],
        }))
        .unwrap();
        let rendered = NetworkPolicies::generate(&policy, ["ceres"], &ToolIdsByGroup::new())
            .remove(0)
            .1;
        assert_eq!(rendered.spec.as_ref().unwrap().ingress, Some(Vec::new()));
        // K8s stores the policy without the empty ingress list
        let mut stored = rendered.clone();
        stored.spec.as_mut().unwrap().ingress = None;
        assert!(policies_equal(&stored, &rendered));
        // a policy that differs in its rules is still unequal
        let mut changed = stored.clone();
        changed.spec.as_mut().unwrap().egress = None;
        assert!(!policies_equal(&changed, &rendered));
    }

    /// Policies are reconciled with K8s before the first reconcile and whenever the tools
    /// tool-name entries resolve to change, and not otherwise
    #[test]
    fn reconcile_when_resolved_tools_change() {
        let mut cache = NetworkPolicyCache::default();
        let ids = tool_ids("ghidra-cli", &[("ceres", Uuid::new_v4())]);
        // nothing has been reconciled yet
        assert!(cache.tools_changed(&ids));
        assert!(cache.tools_changed(&ToolIdsByGroup::new()));
        // after a reconcile only a change counts
        cache.reconciled_tool_ids = Some(ids.clone());
        assert!(!cache.tools_changed(&ids));
        let renamed = tool_ids("ghidra-cli", &[("ceres", Uuid::new_v4())]);
        assert!(cache.tools_changed(&renamed));
        assert!(cache.tools_changed(&ToolIdsByGroup::new()));
    }
}
