//! Handles groups commands
use thorium::models::{GroupRequest, GroupUsersRequest};
use thorium::{Error, Thorium};

use crate::args::groups::{CreateGroup, DescribeGroups, GetGroups, Groups};
use crate::args::{Args, DescribeCommand};
use crate::utils;

/// Get and print a list of groups to which the user belongs
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The [`GetGroups`] command that was run
async fn get(thorium: Thorium, cmd: &GetGroups) -> Result<(), Error> {
    // get the current user's groups
    let mut groups = thorium.users.info().await?.groups;
    if cmd.alpha {
        // alphabetize if the flag was set
        groups.sort_unstable();
    }
    for group in &groups {
        println!("{group}");
    }
    Ok(())
}

/// Describe groups by displaying/saving all of their JSON-formatted details
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The [`DescribeGroups`] command to run
async fn describe(thorium: Thorium, cmd: &DescribeGroups) -> Result<(), Error> {
    cmd.describe(&thorium).await
}

/// Build the request that creates the group a command describes
///
/// # Arguments
///
/// * `cmd` - The [`CreateGroup`] command that was run
fn create_request(cmd: &CreateGroup) -> GroupRequest {
    // start from the group's name
    let mut request = GroupRequest::new(&cmd.name);
    // add the description when one was given
    if let Some(description) = &cmd.description {
        request = request.description(description);
    }
    // add each extra owner; the server adds the caller as an owner itself
    let owners = cmd
        .owner
        .iter()
        .fold(GroupUsersRequest::default(), |owners, owner| {
            owners.direct(owner)
        });
    request.owners(owners)
}

/// Create a group owned by the current user
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The [`CreateGroup`] command that was run
async fn create(thorium: Thorium, cmd: &CreateGroup) -> Result<(), Error> {
    // build the request and create the group
    let request = create_request(cmd);
    thorium.groups.create(&request).await?;
    // tell the user the group exists now
    println!("Group '{}' created", request.name);
    Ok(())
}

/// Handle all groups commands or print groups docs
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The groups command to execute
pub async fn handle(args: &Args, cmd: &Groups) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // call the right groups handler
    match cmd {
        Groups::Get(cmd) => get(thorium, cmd).await,
        Groups::Describe(cmd) => describe(thorium, cmd).await,
        Groups::Create(cmd) => create(thorium, cmd).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a create command
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the group
    /// * `description` - The description of the group
    /// * `owners` - The extra owners of the group
    fn command(name: &str, description: Option<&str>, owners: &[&str]) -> CreateGroup {
        CreateGroup {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            owner: owners.iter().map(|owner| (*owner).to_owned()).collect(),
        }
    }

    /// The request carries the name, the description and each owner once
    #[test]
    fn create_request_carries_every_field() {
        // a repeated owner is only added once
        let request = create_request(&command(
            "corn",
            Some("Corn things"),
            &["bob", "sarah", "bob"],
        ));
        assert_eq!(request.name, "corn");
        assert_eq!(request.description.as_deref(), Some("Corn things"));
        let mut owners: Vec<&str> = request.owners.direct.iter().map(String::as_str).collect();
        owners.sort_unstable();
        assert_eq!(owners, ["bob", "sarah"]);
        // nothing else is requested, so the server adds only the caller as an owner
        assert!(request.managers.direct.is_empty() && request.users.direct.is_empty());
        assert!(request.monitors.direct.is_empty());
        // a bare create has no description and no extra owners
        let bare = create_request(&command("corn", None, &[]));
        assert!(bare.description.is_none() && bare.owners.direct.is_empty());
    }
}
