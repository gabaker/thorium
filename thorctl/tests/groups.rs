//! Tests that `thorctl groups create` makes groups owned by the caller

mod common;

use common::{TestUser, assert_success, stdout, unique};
use thorium::Error;

/// `groups create` makes a group with a description, owned by the caller and any extra owners
#[tokio::test]
async fn groups_create_makes_an_owned_group() -> Result<(), Error> {
    // a user to create the group and another to co-own it
    let user = TestUser::new("groups-create").await?;
    let other = TestUser::new("groups-create-owner").await?;
    // create a group with a description and the other user as an owner
    let name = unique("ctlnew");
    let args = [
        "groups",
        "create",
        &name,
        "--description",
        "Made by thorctl",
        "--owner",
        &other.username,
    ];
    let output = user.run("groups-create", &args).await;
    assert_success(&output);
    assert!(stdout(&output).contains(&name), "{}", stdout(&output));
    // the group exists with the requested description and both owners
    let group = user.client.groups.get(&name).await?;
    assert_eq!(group.description.as_deref(), Some("Made by thorctl"));
    assert!(group.owners.direct.contains(&user.username));
    assert!(group.owners.direct.contains(&other.username));
    // the new group is listed for its creator
    let output = user.run("groups-get", &["groups", "get"]).await;
    assert_success(&output);
    assert!(stdout(&output).lines().any(|line| line == name));
    Ok(())
}

/// `groups create` fails for a group that already exists and for an invalid name
#[tokio::test]
async fn groups_create_refuses_bad_requests() -> Result<(), Error> {
    let user = TestUser::new("groups-create-refused").await?;
    // creating the user's existing group again fails
    let output = user
        .run("groups-create-exists", &["groups", "create", &user.group])
        .await;
    assert!(!output.status.success());
    // a name with uppercase letters is not a valid group name
    let output = user
        .run("groups-create-invalid", &["groups", "create", "NotValid"])
        .await;
    assert!(!output.status.success());
    // neither failure left a group behind
    assert!(user.client.groups.get("NotValid").await.is_err());
    Ok(())
}
