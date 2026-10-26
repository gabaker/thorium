//! Handles network policies commands

use std::path::Path;
use thorium::{
    CtlConf, Error, Thorium,
    conf::BaseNetworkPolicy,
    models::{NetworkPolicyListLine, NetworkPolicyRequest, NetworkPolicyUpdate},
};

use crate::args::{Args, DescribeCommand, SearchParameterized};
use crate::utils;
use crate::{
    args::network_policies::{
        CreateNetworkPolicy, DefaultNetworkPolicies, DeleteNetworkPolicy, DescribeNetworkPolicies,
        GetNetworkPolicies, NetworkPolicies, NetworkPolicyFileFormat, NetworkPolicyRules,
        UpdateNetworkPolicy, VerifyNetworkPolicies,
    },
    err_not_admin,
};

struct NetworkPolicyLine;

impl NetworkPolicyLine {
    fn header() {
        println!("{:<64} | {:<36}", "POLICY NAME", "ID");
        println!("{:-<65}+{:-<37}", "", "");
    }

    fn print(network_policy_line: &NetworkPolicyListLine) {
        println!(
            "{:<64} | {:<37}",
            network_policy_line.name, network_policy_line.id
        );
    }
}

/// Get a list of network policies
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get network policies command that was run
async fn get(thorium: Thorium, cmd: &GetNetworkPolicies) -> Result<(), Error> {
    let opts = cmd.build_network_policy_opts();
    let mut cursor = thorium.network_policies.list(&opts).await?;
    NetworkPolicyLine::header();
    loop {
        for network_policy_line in cursor.data.drain(..) {
            NetworkPolicyLine::print(&network_policy_line);
        }
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    Ok(())
}

/// Get a list of default network policies for a given group
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The default network policies command that was run
async fn get_default(thorium: Thorium, cmd: &DefaultNetworkPolicies) -> Result<(), Error> {
    let default_policies = thorium.network_policies.get_all_default(&cmd.group).await?;
    NetworkPolicyLine::header();
    for policy_list_line in default_policies {
        NetworkPolicyLine::print(&policy_list_line);
    }
    Ok(())
}

/// Describe network policies by displaying/saving their JSON-formatted details
///
/// * `thorium` - The Thorium client
/// * `cmd` - The [`DescribeNetworkPolicies`] command to execute
async fn describe(thorium: Thorium, cmd: &DescribeNetworkPolicies) -> Result<(), Error> {
    cmd.describe(&thorium).await
}

/// Create a network policy
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf`- The Thorctl config
/// * `cmd` - The create network policy command that was run
async fn create(thorium: Thorium, conf: CtlConf, cmd: &CreateNetworkPolicy) -> Result<(), Error> {
    // make a create request from the command
    let req = cmd.to_req(&conf)?;
    // send the request
    err_not_admin!(
        thorium.network_policies.create(req).await,
        "create network policies"
    );
    Ok(())
}

/// Delete a network policy
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete network policy command that was run
async fn delete(thorium: Thorium, cmd: &DeleteNetworkPolicy) -> Result<(), Error> {
    // ask on the real terminal
    delete_asking(&thorium, cmd, &utils::Terminal).await
}

/// Delete a network policy, confirming with `asker` unless `--skip-confirm` was given
///
/// A network policy is shared by every group it is in, so the question names the policy and
/// the groups that hold it; deleting it removes it for all of them.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete network policy command that was run
/// * `asker` - Where the confirmation answer comes from
async fn delete_asking<A: utils::Asker>(
    thorium: &Thorium,
    cmd: &DeleteNetworkPolicy,
    asker: &A,
) -> Result<(), Error> {
    if !cmd.skip_confirm {
        // refuse before reading anything when there is no one to ask
        utils::require_asker(asker, "--skip-confirm")?;
        // read the policy so the question names it and the groups that hold it
        let policy = thorium.network_policies.get(&cmd.name, cmd.id).await?;
        // a policy still used by images is worth saying
        let used = policy.used_by.values().map(Vec::len).sum::<usize>();
        let used = if used > 0 {
            format!("; {used} image(s) use it")
        } else {
            String::new()
        };
        // the policy is shared, so deleting it removes it from every group that holds it
        let question = format!(
            "Delete network policy '{}'{} for all of its groups{used}?",
            policy.name,
            utils::policy_suffix(&policy.groups, Some(&policy.id)),
        );
        // ask, and stop when the answer is no
        if !utils::confirm_or_skip(asker, &question, "--skip-confirm", false)? {
            return Ok(());
        }
    }
    // send the request
    err_not_admin!(
        thorium.network_policies.delete(&cmd.name, cmd.id).await,
        "delete network policies"
    );
    Ok(())
}

/// Update a network policy
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config
/// * `cmd` - The update network policy command that was run
async fn update(thorium: Thorium, conf: CtlConf, cmd: &UpdateNetworkPolicy) -> Result<(), Error> {
    // make a update request from the command
    let update = cmd.to_update(&conf)?;
    // send the request
    err_not_admin!(
        thorium
            .network_policies
            .update(&cmd.name, cmd.id, &update)
            .await,
        "update network policies"
    );
    Ok(())
}

/// Parse the rules for ingress and egress from a rules file
///
/// # Arguments
///
/// * `path` - The path to the rules file
/// * `format` - The format the rules file is in
/// * `api_url` - The URL to the API to use to provide a link to formatting information
///               on error
fn parse_rules_file(
    path: &Path,
    format: &NetworkPolicyFileFormat,
    api_url: &str,
) -> Result<NetworkPolicyRules, Error> {
    // open the rules file
    let file = std::fs::File::open(path).map_err(|err| {
        Error::new(format!(
            "Failed to open rules file at '{}': {}",
            path.to_string_lossy(),
            err
        ))
    })?;
    // try to deserialize the rules file
    let rules: NetworkPolicyRules = match format {
            NetworkPolicyFileFormat::Json => serde_json::from_reader(&file).map_err(|err| {
                Error::new(format!(
                    "Failed to deserialize JSON rules file at '{}': {}\n\n\
                    For formatting guidance on rules files, view the docs at '{}/docs/user/admins/network_policies.html#the-rules-file'",
                    path.to_string_lossy(),
                    err,
                    api_url
                ))
            })?,
            NetworkPolicyFileFormat::Yaml => serde_norway::from_reader(&file).map_err(|err| {
                Error::new(format!(
                    "Failed to deserialize YAML rules file at '{}': {}\n\n\
                    For formatting guidance on rules files, view the docs at '{}/docs/user/admins/network_policies.html#the-rules-file'",
                    path.to_string_lossy(),
                    err,
                    api_url
                ))
            })?,
        };
    Ok(rules)
}

impl CreateNetworkPolicy {
    /// Generate a [`NetworkPolicyRequest`] from the options set in a
    /// `CreateNetworkPolicy` command
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorctl config used for Thoradm
    fn to_req(&self, conf: &CtlConf) -> Result<NetworkPolicyRequest, Error> {
        // parse the rules file if we were given one
        let rules = self
            .rules_file
            .as_ref()
            .map(|rules_file| parse_rules_file(rules_file, &self.format, &conf.keys.api))
            .transpose()?;
        // unwrap the outer option
        let (ingress, egress) = match rules {
            Some(rules) => (rules.ingress, rules.egress),
            None => (None, None),
        };
        // create the network policy request
        Ok(NetworkPolicyRequest {
            name: self.name.clone(),
            groups: (!self.groups.is_empty())
                .then_some(self.groups.clone())
                .ok_or(Error::new("Groups cannot be empty!"))?,
            ingress,
            egress,
            forced_policy: self.forced,
            default_policy: self.default,
        })
    }
}

impl UpdateNetworkPolicy {
    /// Generate a [`NetworkPolicyUpdate`] from the options set in a
    /// `UpdateNetworkPolicy` command
    fn to_update(&self, conf: &CtlConf) -> Result<NetworkPolicyUpdate, Error> {
        // parse the rules file
        let (add_ingress, add_egress) = match &self.opts.rules_file {
            Some(rules_file) => {
                let rules = parse_rules_file(rules_file, &self.opts.format, &conf.keys.api)?;
                (
                    rules.ingress.unwrap_or_default(),
                    rules.egress.unwrap_or_default(),
                )
            }
            None => (vec![], vec![]),
        };
        Ok(NetworkPolicyUpdate {
            new_name: self.opts.new_name.clone(),
            add_groups: self.opts.add_groups.clone(),
            remove_groups: self.opts.remove_groups.clone(),
            add_ingress,
            remove_ingress: self.opts.remove_ingress.clone(),
            clear_ingress: self.opts.clear_ingress,
            deny_all_ingress: self.opts.deny_all_ingress,
            add_egress,
            remove_egress: self.opts.remove_egress.clone(),
            deny_all_egress: self.opts.deny_all_egress,
            clear_egress: self.opts.clear_egress,
            forced_policy: self.opts.forced,
            default_policy: self.opts.default,
        })
    }
}

/// Verify a file containing rules or base network policies is valid
///
/// # Arguments
///
/// * `cmd` - The verify network policies command that was run
fn verify(cmd: &VerifyNetworkPolicies) -> Result<(), Error> {
    match cmd {
        VerifyNetworkPolicies::Rules(cmd) => {
            // open the rules file
            let file = std::fs::File::open(&cmd.rules_file).map_err(|err| {
                Error::new(format!(
                    "Unable to open rules file '{}': {}",
                    cmd.rules_file.to_string_lossy(),
                    err
                ))
            })?;
            // parse the rules file depending on the format
            let rules: NetworkPolicyRules = match cmd.format {
                NetworkPolicyFileFormat::Yaml => {
                    serde_norway::from_reader(&file).map_err(|err| {
                        Error::new(format!(
                            "Invalid rules file '{}': {}",
                            cmd.rules_file.to_string_lossy(),
                            err,
                        ))
                    })?
                }
                NetworkPolicyFileFormat::Json => serde_json::from_reader(&file).map_err(|err| {
                    Error::new(format!(
                        "Invalid rules file '{}': {}",
                        cmd.rules_file.to_string_lossy(),
                        err,
                    ))
                })?,
            };
            // print what we succesfully parsed
            println!("Rules:\n\n{rules:#?}");
        }
        VerifyNetworkPolicies::Base(cmd) => {
            // open the base network policies file
            let file = std::fs::File::open(&cmd.policies_file).map_err(|err| {
                Error::new(format!(
                    "Unable to open base policies file '{}': {}",
                    cmd.policies_file.to_string_lossy(),
                    err
                ))
            })?;
            // parse the base network policies from YAML
            let base_policies: Vec<BaseNetworkPolicy> =
                serde_norway::from_reader(&file).map_err(|err| {
                    Error::new(format!(
                        "Invalid base network policies file '{}': {}",
                        cmd.policies_file.to_string_lossy(),
                        err,
                    ))
                })?;
            // print what we succesfully parsed
            println!("Base Network Policies:\n\n{base_policies:#?}");
        }
    }
    Ok(())
}

pub async fn handle(args: &Args, network_policies: &NetworkPolicies) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    match network_policies {
        NetworkPolicies::Get(cmd) => get(thorium, cmd).await,
        NetworkPolicies::Default(cmd) => get_default(thorium, cmd).await,
        NetworkPolicies::Describe(cmd) => describe(thorium, cmd).await,
        NetworkPolicies::Create(cmd) => create(thorium, conf, cmd).await,
        NetworkPolicies::Delete(cmd) => delete(thorium, cmd).await,
        NetworkPolicies::Update(cmd) => update(thorium, conf, cmd).await,
        NetworkPolicies::Verify(cmd) => verify(cmd),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use clap::Parser;
    use thorium::models::NetworkPolicy;
    use uuid::Uuid;

    use super::delete_asking;
    use crate::args::network_policies::DeleteNetworkPolicy;
    use crate::utils::run_wait::testing::{Requests, client, mock};
    use crate::utils::testing::Scripted;

    /// A network policy shared by two groups
    fn shared_policy() -> NetworkPolicy {
        NetworkPolicy {
            name: "allow-dns".to_owned(),
            id: Uuid::nil(),
            k8s_name: format!("allow-dns-{}", Uuid::nil()),
            groups: vec!["corn".to_owned(), "wheat".to_owned()],
            created: Utc::now(),
            ingress: None,
            egress: None,
            forced_policy: false,
            default_policy: false,
            used_by: HashMap::new(),
        }
    }

    /// Start a mock API that serves the shared policy and accepts its deletion
    async fn policy_api() -> (String, Requests) {
        mock(|line, _| {
            if line.starts_with("GET /api/network-policies/allow-dns") {
                (200, serde_json::to_string(&shared_policy()).unwrap())
            } else if line.starts_with("DELETE /api/network-policies/allow-dns") {
                (204, String::new())
            } else {
                (404, r#"{"error":"not found"}"#.to_owned())
            }
        })
        .await
    }

    /// Parse a `network-policies delete` command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `network-policies delete`
    fn delete_cmd(args: &[&str]) -> DeleteNetworkPolicy {
        // prepend the subcommand name and parse
        let mut full = vec!["delete"];
        full.extend_from_slice(args);
        DeleteNetworkPolicy::try_parse_from(full).unwrap()
    }

    /// Count the requests whose line starts with a prefix
    ///
    /// # Arguments
    ///
    /// * `requests` - The mock's request log
    /// * `prefix` - What the request lines start with
    fn count(requests: &Requests, prefix: &str) -> usize {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _)| line.starts_with(prefix))
            .count()
    }

    /// The question names the policy and every group that holds it, and yes deletes it
    #[tokio::test]
    async fn delete_asks_naming_the_groups_then_deletes_on_yes() {
        let (host, requests) = policy_api().await;
        let thorium = client(&host).await;
        let asker = Scripted::answering(true);
        delete_asking(&thorium, &delete_cmd(&["allow-dns"]), &asker)
            .await
            .unwrap();
        let questions = asker.asked.borrow();
        assert_eq!(questions.len(), 1, "{questions:?}");
        for part in ["allow-dns", "corn", "wheat", &Uuid::nil().to_string()] {
            assert!(questions[0].contains(part), "{part} missing: {questions:?}");
        }
        assert_eq!(count(&requests, "DELETE "), 1);
    }

    /// A declined question deletes nothing and is not an error
    #[tokio::test]
    async fn delete_declined_changes_nothing() {
        let (host, requests) = policy_api().await;
        let thorium = client(&host).await;
        let asker = Scripted::answering(false);
        delete_asking(&thorium, &delete_cmd(&["allow-dns"]), &asker)
            .await
            .unwrap();
        assert_eq!(count(&requests, "DELETE "), 0);
    }

    /// `--skip-confirm` and `--yes` delete without asking or reading the policy first
    #[tokio::test]
    async fn delete_skip_confirm_never_asks() {
        let (host, requests) = policy_api().await;
        let thorium = client(&host).await;
        for flag in ["--skip-confirm", "--yes"] {
            let asker = Scripted::no_terminal();
            delete_asking(&thorium, &delete_cmd(&["allow-dns", flag]), &asker)
                .await
                .unwrap();
            assert!(asker.asked.borrow().is_empty());
        }
        assert_eq!(count(&requests, "DELETE "), 2);
        assert_eq!(count(&requests, "GET "), 0);
    }

    /// Without a terminal and without the flag the delete is refused, naming the flag
    #[tokio::test]
    async fn delete_without_a_terminal_is_refused() {
        let (host, requests) = policy_api().await;
        let thorium = client(&host).await;
        let asker = Scripted::no_terminal();
        let err = delete_asking(&thorium, &delete_cmd(&["allow-dns"]), &asker)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--skip-confirm"), "{err}");
        assert_eq!(count(&requests, "DELETE "), 0);
    }
}
