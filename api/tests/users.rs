//! Tests the users routes in Thorium

use base64::Engine;
use chrono::{Duration, Utc};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use thorium::client::{ClientSettings, Users};
use thorium::models::{
    AiEndpointUpdate, AiSettings, AiSettingsUpdate, AwsRoleSource, BedrockEndpointKind,
    CredentialKind, FileDeleteOpts, FileDownloadOpts, Group, GroupAllowed, GroupAllowedUpdate,
    GroupRequest, GroupUpdate, GroupUsersUpdate, ImageScaler, ModelApi, ModelBan, ModelBanKind,
    ModelBanUpdate, ModelCatalogEntryRequest, ModelEndpointEntityRequest,
    ModelProfileEntityRequest, ModelProvider, ModelSecret, NotificationLevel, NotificationParams,
    NotificationRequest, NotificationType, RunInputs, RunTokenScope, SampleRequest, ScopedToken,
    ScopedTokenRequest, ScopedTokenRole, ScopedTokenUpdate, ScrubbedUser, TagRequest,
    ToolRunRequest, UserCreate, UserCredentialRef, UserModelEndpoint, UserModelSettings,
    UserModelSettingsUpdate, UserRole, UserSettings, UserSettingsUpdate, UserUpdate,
};
use thorium::test_utilities::generators::GroupRoles;
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail, is, is_not};
use uuid::Uuid;

/// Get a connection to the test API's Redis database
async fn users_redis() -> redis::aio::MultiplexedConnection {
    // reuse the harness's pool so the connection selects the configured database
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let pooled = pool.get().await.expect("a Redis connection");
    pooled.clone()
}

/// A tool run's token for a new developer, with the developer and the admin client
async fn run_token_fixture() -> Result<(Thorium, generators::TestUser, ScopedToken), Error> {
    // a developer who owns a group with an Image tool and a file
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let runner = generators::named_user_with_role(
        &admin,
        &generators::readable_name("autonoe", &suffix),
        UserRole::Developer {
            k8s: true,
            bare_metal: false,
            windows: false,
            external: false,
            kvm: false,
        },
    )
    .await?;
    let group = generators::god_group(
        &admin,
        "bellona",
        &suffix,
        &GroupRoles::default().owner(&runner.username),
    )
    .await?;
    let (_, image) =
        thorium::test_utilities::runs::image_tool(&[group.as_str()], ImageScaler::K8s, &admin)
            .await?;
    let sha256 = runner
        .client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    // run the tool and read the run's token as the admin
    let created = runner
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let token = admin.reactions.run_token(&group, created.id).await?;
    Ok((admin, runner, token))
}

/// A scoped token stored before run scopes existed loads with no run and authenticates as
/// before, and a run token round trips its run with a value that never rotates
#[tokio::test]
async fn scoped_tokens_without_runs_load() -> Result<(), Error> {
    // a user and a group they own
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let owner = generators::moon_user(&admin, "erinome", &suffix).await?;
    let group = generators::god_group(
        &admin,
        "pales",
        &suffix,
        &GroupRoles::default().owner(&owner.username),
    )
    .await?;
    // write a scoped token the way it was stored before run scopes existed
    let ns = &test_utilities::CONF.thorium.namespace;
    let value = Uuid::new_v4().simple().to_string();
    let name = format!("legacy-{suffix}");
    let legacy = serde_json::json!({
        "name": name,
        "owner": owner.username,
        "role": "User",
        "groups": [group],
        "token": value,
        "token_expiration": Utc::now() + Duration::days(1),
        "expires": null,
    });
    let mut conn = users_redis().await;
    let _: () = redis::pipe()
        .cmd("hset")
        .arg(format!("{ns}:scoped_tokens:{}", owner.username))
        .arg(&name)
        .arg(legacy.to_string())
        .cmd("hset")
        .arg(format!("{ns}:scoped_token_map"))
        .arg(&value)
        .arg(serde_json::to_string(&(&owner.username, &name))?)
        .query_async(&mut conn)
        .await
        .expect("write a legacy scoped token");
    // it lists with no run and authenticates as the owner within its group
    let listed = owner.client.users.get_scoped_token(&name).await?;
    is!(listed.run.is_none(), true);
    let scoped = scoped_client(&owner.client, &value).await?;
    let info = scoped.users.info().await?;
    is!(info.username, owner.username.clone());
    is!(info.groups, vec![group.clone()]);
    // a run token keeps its run, and its value expires with the token and is never rotated
    let (admin, runner, token) = run_token_fixture().await?;
    let scope = token.run.clone().expect("a run scope");
    let listed = runner.client.users.get_scoped_token(&token.name).await?;
    is!(listed.run, Some(scope.clone()));
    is!(Some(listed.token_expiration), listed.expires);
    is!(listed.token, token.token.clone());
    // the run token authenticates within its groups
    let scoped = scoped_client(&admin, &token.token).await?;
    is!(scoped.users.info().await?.groups, token.groups.clone());
    Ok(())
}

/// Users can't create run-scoped tokens themselves
#[tokio::test]
async fn scoped_token_create_refuses_runs() -> Result<(), Error> {
    // a user and a group they own
    let admin = test_utilities::admin_client().await?;
    let client = generators::client(&admin).await?;
    let group = create_group(&client).await?;
    // a request with a run scope is refused and creates nothing
    let mut req = ScopedTokenRequest::new("sneaky-run").group(&group);
    req.run = Some(RunTokenScope {
        reaction: Uuid::new_v4(),
        tool_instances: vec![Uuid::new_v4()],
    });
    req.expires = Some(Utc::now() + Duration::hours(1));
    fail!(client.users.create_scoped_token(&req).await, 400);
    fail!(client.users.get_scoped_token("sneaky-run").await, 404);
    Ok(())
}

/// Every update of a run token is refused with its owner's full token and leaves it unchanged,
/// while the same updates still apply to an ordinary scoped token
#[tokio::test]
async fn run_tokens_are_immutable() -> Result<(), Error> {
    // a run token and an ordinary token of the same runner
    let (_, runner, token) = run_token_fixture().await?;
    let group = token.groups[0].clone();
    let plain = runner
        .client
        .users
        .create_scoped_token(&ScopedTokenRequest::new("plain-token").group(&group))
        .await?;
    // every kind of update
    let updates = [
        ScopedTokenUpdate::default().role(ScopedTokenRole::User),
        ScopedTokenUpdate::default().add_group(&group),
        ScopedTokenUpdate::default().remove_group(&group),
        ScopedTokenUpdate::default().expires(Utc::now() + Duration::days(30)),
        ScopedTokenUpdate::default().clear_expires(),
    ];
    for update in &updates {
        // the run token refuses it and is unchanged
        fail!(
            runner
                .client
                .users
                .update_scoped_token(&token.name, update)
                .await,
            400
        );
        let stored = runner.client.users.get_scoped_token(&token.name).await?;
        is!(stored.groups, token.groups.clone());
        is!(stored.role, token.role.clone());
        is!(stored.expires, token.expires);
        is!(stored.run, token.run.clone());
    }
    // the ordinary token still takes each update but the group removal that would empty it
    for update in updates
        .iter()
        .filter(|update| update.remove_groups.is_empty())
    {
        runner
            .client
            .users
            .update_scoped_token(&plain.name, update)
            .await?;
    }
    is!(
        runner
            .client
            .users
            .get_scoped_token(&plain.name)
            .await?
            .expires,
        None::<chrono::DateTime<Utc>>
    );
    Ok(())
}

/// An admin creates a notification for a named user, who then lists and dismisses it while no
/// other user sees it; a non-admin is refused and an unknown user is not found, with nothing
/// written for either
#[tokio::test]
async fn admin_creates_user_notification() -> Result<(), Error> {
    // an admin and two plain users with readable names
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let europa = generators::moon_user(&admin, "europa", &suffix).await?;
    let ganymede = generators::moon_user(&admin, "ganymede", &suffix).await?;
    // the admin notifies europa
    let req = NotificationRequest::<thorium::models::User>::new(
        "workflow `minerva-flow` #r1 is Ready",
        NotificationLevel::Info,
    );
    admin
        .users
        .create_notification(&europa.username, &req, &NotificationParams::default())
        .await?;
    // europa lists exactly that notification
    let notes = europa.client.users.notifications().await?;
    is!(notes.len(), 1);
    is!(notes[0].msg, req.msg);
    is!(notes[0].level, NotificationLevel::Info);
    is!(notes[0].key, europa.username);
    // no other user sees it
    let others = ganymede.client.users.notifications().await?;
    assert!(
        others.is_empty(),
        "another user saw the notification: {others:?}"
    );
    // a non-admin can't create one, for themselves or anyone else, and nothing is written
    let resp = ganymede
        .client
        .users
        .create_notification(&europa.username, &req, &NotificationParams::default())
        .await;
    fail!(resp, 401);
    let resp = ganymede
        .client
        .users
        .create_notification(&ganymede.username, &req, &NotificationParams::default())
        .await;
    fail!(resp, 401);
    is!(europa.client.users.notifications().await?.len(), 1);
    is!(ganymede.client.users.notifications().await?.len(), 0);
    // an unknown user is not found and nothing is written under that name
    let unknown = generators::readable_name("callisto", &suffix);
    let resp = admin
        .users
        .create_notification(&unknown, &req, &NotificationParams::default())
        .await;
    fail!(resp, 404);
    is!(
        test_utilities::notification_rows(NotificationType::Users, &unknown).await?,
        0
    );
    // europa dismisses the notification and it's gone
    europa.client.users.delete_notification(notes[0].id).await?;
    assert!(europa.client.users.notifications().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn delete() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&client).await?;
    // get our users info
    let info = client.users.info().await?;
    // delete our user
    client.users.delete(&info.username).await?;
    Ok(())
}

/// Create a group owned by the given clients user
///
/// # Arguments
///
/// * `client` - The client to create a group with
async fn create_group(client: &Thorium) -> Result<String, Error> {
    // generate a random lowercase group name
    let name = Uuid::new_v4().simple().to_string();
    // build a group request for this group
    let req = GroupRequest::new(&name);
    // create this group
    client.groups.create(&req).await?;
    Ok(name)
}

/// Build a client that authenticates with a scoped token
///
/// # Arguments
///
/// * `client` - The client to get a host from
/// * `token` - The scoped token value to authenticate with
async fn scoped_client(client: &Thorium, token: &str) -> Result<Thorium, Error> {
    // build a client that uses this scoped token
    Thorium::build(client.host.clone())
        .token(token)
        .build()
        .await
}

#[tokio::test]
async fn scoped_token_crud() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // create two groups owned by this user
    let in_scope = create_group(&client).await?;
    let out_of_scope = create_group(&client).await?;
    // build a scoped token request limited to one group
    let req = ScopedTokenRequest::new("crud-token").group(&in_scope);
    // create our scoped token
    let scoped = client.users.create_scoped_token(&req).await?;
    // make sure our scoped token has the right settings
    is!(scoped.name, "crud-token");
    is!(scoped.groups, vec![in_scope.clone()]);
    // build a client that authenticates with our scoped token
    let scoped_thorium = scoped_client(&client, &scoped.token).await?;
    // make sure whoami shows only in scope groups and the scoped tokens value
    let info = scoped_thorium.users.info().await?;
    is!(info.groups, vec![in_scope.clone()]);
    is!(info.token, scoped.token);
    // make sure we can get our in scope group
    scoped_thorium.groups.get(&in_scope).await?;
    // but not our out of scope group
    fail!(scoped_thorium.groups.get(&out_of_scope).await, 401);
    // make sure our primary token can still see both groups
    client.groups.get(&in_scope).await?;
    client.groups.get(&out_of_scope).await?;
    // make sure our scoped token shows up when listing
    let listed = client.users.list_scoped_tokens().await?;
    is!(listed.len(), 1);
    // and when getting it by name
    let got = client.users.get_scoped_token("crud-token").await?;
    is!(got.token, scoped.token);
    // delete our scoped token
    client.users.delete_scoped_token("crud-token").await?;
    // make sure our scoped token no longer works
    fail!(scoped_thorium.users.info().await, 401);
    // and is no longer listed
    let listed = client.users.list_scoped_tokens().await?;
    is!(listed.len(), 0);
    Ok(())
}

#[tokio::test]
async fn scoped_token_create_invalid() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // create a group owned by this user
    let group = create_group(&client).await?;
    // make sure groups outside our own are rejected
    let req = ScopedTokenRequest::new("invalid-groups").group("not-our-group");
    fail!(client.users.create_scoped_token(&req).await, 401);
    // make sure empty scopes are rejected
    let req = ScopedTokenRequest::new("empty-scope");
    fail!(client.users.create_scoped_token(&req).await, 400);
    // make sure expirations in the past are rejected
    let req = ScopedTokenRequest::new("past-expires")
        .group(&group)
        .expires(Utc::now() - Duration::days(1));
    fail!(client.users.create_scoped_token(&req).await, 400);
    // make sure invalid names are rejected
    let req = ScopedTokenRequest::new("NotLowercase").group(&group);
    fail!(client.users.create_scoped_token(&req).await, 400);
    // make sure duplicate names are rejected
    let req = ScopedTokenRequest::new("duplicate").group(&group);
    client.users.create_scoped_token(&req).await?;
    fail!(client.users.create_scoped_token(&req).await, 409);
    Ok(())
}

#[tokio::test]
async fn scoped_token_update() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // create two groups owned by this user
    let group_a = create_group(&client).await?;
    let group_b = create_group(&client).await?;
    // create a scoped token limited to our first group
    let req = ScopedTokenRequest::new("update-token").group(&group_a);
    let scoped = client.users.create_scoped_token(&req).await?;
    // build a client that authenticates with our scoped token
    let scoped_thorium = scoped_client(&client, &scoped.token).await?;
    // add our second group to this scoped tokens scope
    let update = ScopedTokenUpdate::default().add_group(&group_b);
    let updated = client
        .users
        .update_scoped_token("update-token", &update)
        .await?;
    // make sure our scope now contains both groups
    is!(updated.groups.len(), 2);
    is!(updated.groups.contains(&group_b), true);
    // make sure our scoped tokens value was not changed
    is!(updated.token, scoped.token);
    // make sure our scoped token still authenticates after the update
    let info = scoped_thorium.users.info().await?;
    is!(info.groups.len(), 2);
    // and can now access our second group
    scoped_thorium.groups.get(&group_b).await?;
    // remove our first group from this scoped tokens scope
    let update = ScopedTokenUpdate::default().remove_group(&group_a);
    let updated = client
        .users
        .update_scoped_token("update-token", &update)
        .await?;
    // make sure our scope only contains our second group
    is!(updated.groups, vec![group_b.clone()]);
    // and our first group is no longer accessible with our scoped token
    fail!(scoped_thorium.groups.get(&group_a).await, 401);
    // set an expiration date for this scoped token
    let expires = Utc::now() + Duration::days(1);
    let update = ScopedTokenUpdate::default().expires(expires);
    let updated = client
        .users
        .update_scoped_token("update-token", &update)
        .await?;
    // make sure our expiration date was set
    is!(updated.expires, Some(expires));
    // clear this scoped tokens expiration date
    let update = ScopedTokenUpdate::default().clear_expires();
    let updated = client
        .users
        .update_scoped_token("update-token", &update)
        .await?;
    // make sure our expiration date was cleared
    is!(updated.expires.is_none(), true);
    Ok(())
}

#[tokio::test]
async fn scoped_token_update_invalid() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // create a group owned by this user
    let group = create_group(&client).await?;
    // create a scoped token limited to our group
    let req = ScopedTokenRequest::new("update-invalid").group(&group);
    client.users.create_scoped_token(&req).await?;
    // make sure updates to unknown scoped tokens are rejected
    let update = ScopedTokenUpdate::default().add_group(&group);
    fail!(
        client.users.update_scoped_token("unknown", &update).await,
        404
    );
    // make sure adding groups outside our own is rejected
    let update = ScopedTokenUpdate::default().add_group("not-our-group");
    fail!(
        client
            .users
            .update_scoped_token("update-invalid", &update)
            .await,
        401
    );
    // make sure removing all groups is rejected
    let update = ScopedTokenUpdate::default().remove_group(&group);
    fail!(
        client
            .users
            .update_scoped_token("update-invalid", &update)
            .await,
        400
    );
    // make sure expirations in the past are rejected
    let update = ScopedTokenUpdate::default().expires(Utc::now() - Duration::days(1));
    fail!(
        client
            .users
            .update_scoped_token("update-invalid", &update)
            .await,
        400
    );
    // make sure setting and clearing an expiration at once is rejected
    let update = ScopedTokenUpdate::default()
        .expires(Utc::now() + Duration::days(1))
        .clear_expires();
    fail!(
        client
            .users
            .update_scoped_token("update-invalid", &update)
            .await,
        400
    );
    // make sure empty updates are rejected
    let update = ScopedTokenUpdate::default();
    fail!(
        client
            .users
            .update_scoped_token("update-invalid", &update)
            .await,
        400
    );
    Ok(())
}

#[tokio::test]
async fn scoped_token_restricted_routes() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // create a group owned by this user
    let group = create_group(&client).await?;
    // create a scoped token for this user
    let req = ScopedTokenRequest::new("restricted").group(&group);
    let scoped = client.users.create_scoped_token(&req).await?;
    // build a client that authenticates with our scoped token
    let scoped_thorium = scoped_client(&client, &scoped.token).await?;
    // get our users info
    let info = client.users.info().await?;
    // make sure scoped tokens cannot manage scoped tokens
    let sneaky = ScopedTokenRequest::new("sneaky").group(&group);
    fail!(scoped_thorium.users.create_scoped_token(&sneaky).await, 401);
    fail!(scoped_thorium.users.list_scoped_tokens().await, 401);
    fail!(
        scoped_thorium.users.get_scoped_token("restricted").await,
        401
    );
    let sneaky_update = ScopedTokenUpdate::default().add_group(&group);
    fail!(
        scoped_thorium
            .users
            .update_scoped_token("restricted", &sneaky_update)
            .await,
        401
    );
    fail!(
        scoped_thorium.users.delete_scoped_token("restricted").await,
        401
    );
    // make sure scoped tokens cannot update users
    let update = UserUpdate {
        password: Some(generators::gen_string(24)),
        email: None,
        role: None,
        settings: None,
    };
    let resp = scoped_thorium.users.update(&info.username, update).await;
    fail!(resp, 401);
    // make sure scoped tokens cannot delete users
    fail!(scoped_thorium.users.delete(&info.username).await, 401);
    Ok(())
}

#[tokio::test]
async fn scoped_token_logout() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // create a group owned by this user
    let group = create_group(&client).await?;
    // create a scoped token for this user
    let req = ScopedTokenRequest::new("logout-token").group(&group);
    let scoped = client.users.create_scoped_token(&req).await?;
    // build a client that authenticates with our scoped token
    let scoped_thorium = scoped_client(&client, &scoped.token).await?;
    // make sure our scoped client works
    scoped_thorium.users.info().await?;
    // logout with our scoped token
    scoped_thorium.users.logout().await?;
    // make sure our old scoped token value no longer works
    fail!(scoped_thorium.users.info().await, 401);
    // but our primary token still works
    client.users.info().await?;
    // get the rotated value with our primary token
    let rotated = client.users.get_scoped_token("logout-token").await?;
    is_not!(rotated.token, scoped.token);
    // make sure the rotated value works
    let rotated_thorium = scoped_client(&client, &rotated.token).await?;
    rotated_thorium.users.info().await?;
    Ok(())
}

/// An admin's `User` token acts as a `User` with only the token's groups
#[tokio::test]
async fn scoped_token_admin_demoted() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a group our admin owns
    let group = create_group(&admin).await?;
    // create another user to get info on
    let users = generators::users(1, &admin).await?;
    // make sure admins can get info on other users
    admin.users.get(&users[0]).await?;
    // create a scoped token as our admin
    let req = ScopedTokenRequest::new("demoted").group(&group);
    let scoped = admin.users.create_scoped_token(&req).await?;
    // build a client that authenticates with our scoped token
    let scoped_thorium = scoped_client(&admin, &scoped.token).await?;
    // make sure our scoped token was demoted to the user role
    let info = scoped_thorium.users.info().await?;
    is!(info.role, UserRole::User);
    is!(info.groups, vec![group.clone()]);
    // make sure our scoped token can no longer get info on other users
    fail!(scoped_thorium.users.get(&users[0]).await, 401);
    // but can still access data in the groups its scoped to
    scoped_thorium.groups.get(&group).await?;
    // clean up our admins scoped token
    admin.users.delete_scoped_token("demoted").await?;
    Ok(())
}

/// Build a user update that only sets a role
///
/// # Arguments
///
/// * `role` - The role to set in this update
fn role_update(role: UserRole) -> UserUpdate {
    UserUpdate {
        password: None,
        email: None,
        role: Some(role),
        settings: None,
    }
}

#[tokio::test]
async fn disabled_user_rejected() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // get our users info
    let info = client.users.info().await?;
    // create a group owned by this user so we can check group routes too
    let group = create_group(&client).await?;
    // disable this user as an admin
    admin
        .users
        .update(&info.username, role_update(UserRole::Disabled))
        .await?;
    // make sure this users token is now rejected with a disabled error
    fail!(client.users.info().await, 401, "has been disabled");
    // make sure other routes are rejected with a disabled error too
    fail!(client.groups.get(&group).await, 401, "has been disabled");
    Ok(())
}

#[tokio::test]
async fn disabled_user_cannot_login() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // generate a username and password we know
    let username = generators::gen_string(24);
    let password = generators::gen_string(64);
    // build a user create blueprint
    let bp = UserCreate::new(&username, &password, "fake@fake.gov").skip_verification();
    // create this user in Thorium
    Users::create(
        &admin.host,
        bp,
        Some(&test_utilities::CONF.thorium.secret_key),
        &ClientSettings::default(),
    )
    .await?;
    // make sure this user can login before being disabled
    let client = Thorium::build(&admin.host)
        .basic_auth(username.clone(), password.clone())
        .build()
        .await?;
    client.users.info().await?;
    // disable this user as an admin
    admin
        .users
        .update(&username, role_update(UserRole::Disabled))
        .await?;
    // make sure logging in with basic auth is rejected with a disabled error
    // drop the client on success since Thorium does not implement Debug
    let resp = Thorium::build(&admin.host)
        .basic_auth(username.clone(), password.clone())
        .build()
        .await
        .map(|_| ());
    fail!(resp, 401, "has been disabled");
    Ok(())
}

#[tokio::test]
async fn disabled_user_scoped_token_rejected() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // get our users info
    let info = client.users.info().await?;
    // create a group owned by this user
    let group = create_group(&client).await?;
    // create a scoped token for this user
    let req = ScopedTokenRequest::new("disabled-scoped").group(&group);
    let scoped = client.users.create_scoped_token(&req).await?;
    // build a client that authenticates with our scoped token
    let scoped_thorium = scoped_client(&client, &scoped.token).await?;
    // make sure our scoped token works before this user is disabled
    scoped_thorium.users.info().await?;
    // disable this user as an admin
    admin
        .users
        .update(&info.username, role_update(UserRole::Disabled))
        .await?;
    // make sure our scoped token is rejected with a disabled error
    fail!(scoped_thorium.users.info().await, 401, "has been disabled");
    Ok(())
}

#[tokio::test]
async fn disabled_user_reenable_restores_access() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // get our users info
    let info = client.users.info().await?;
    // disable this user as an admin
    admin
        .users
        .update(&info.username, role_update(UserRole::Disabled))
        .await?;
    // make sure this user is rejected while disabled
    fail!(client.users.info().await, 401, "has been disabled");
    // re-enable this user with the user role
    admin
        .users
        .update(&info.username, role_update(UserRole::User))
        .await?;
    // make sure the same client works again without re-authenticating
    let info = client.users.info().await?;
    // make sure this users role was set back to user
    is!(info.role, UserRole::User);
    Ok(())
}

#[tokio::test]
async fn create_disabled_user_rejected() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // build a user create blueprint with the disabled role
    let bp = UserCreate::new(
        generators::gen_string(24),
        generators::gen_string(64),
        "fake@fake.gov",
    )
    .skip_verification()
    .role(UserRole::Disabled);
    // make sure creating an already disabled user is rejected
    let resp = Users::create(
        &admin.host,
        bp,
        Some(&test_utilities::CONF.thorium.secret_key),
        &ClientSettings::default(),
    )
    .await;
    fail!(resp, 400, "cannot be created with the Disabled role");
    Ok(())
}

#[tokio::test]
async fn admin_cannot_disable_self() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // generate a username and password for a throwaway admin
    // so we never disable the shared bootstrap admin
    let username = generators::gen_string(24);
    let password = generators::gen_string(64);
    // build a user create blueprint for a throwaway admin
    let bp = UserCreate::new(&username, &password, "fake@fake.gov")
        .skip_verification()
        .admin();
    // create this admin in Thorium
    Users::create(
        &admin.host,
        bp,
        Some(&test_utilities::CONF.thorium.secret_key),
        &ClientSettings::default(),
    )
    .await?;
    // build a client for our throwaway admin
    let throwaway = Thorium::build(&admin.host)
        .basic_auth(username.clone(), password.clone())
        .build()
        .await?;
    // make sure this admin cannot disable their own account
    let resp = throwaway
        .users
        .update(&username, role_update(UserRole::Disabled))
        .await;
    fail!(resp, 400, "your own account");
    // make sure our throwaway admin still has access
    throwaway.users.info().await?;
    Ok(())
}

#[tokio::test]
async fn disabled_role_roundtrip() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a random user
    let users = generators::users(1, &admin).await?;
    // disable this user as an admin
    admin
        .users
        .update(&users[0], role_update(UserRole::Disabled))
        .await?;
    // make sure admins can still see this user and their disabled role
    let info = admin.users.get(&users[0]).await?;
    is!(info.role, UserRole::Disabled);
    Ok(())
}

#[tokio::test]
async fn scoped_token_ephemeral() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // get a user client
    let client = generators::client(&admin).await?;
    // create a group owned by this user
    let group = create_group(&client).await?;
    // create a scoped token that permanently expires almost immediately
    let req = ScopedTokenRequest::new("ephemeral")
        .group(&group)
        .expires(Utc::now() + Duration::seconds(2));
    let scoped = client.users.create_scoped_token(&req).await?;
    // build a client that authenticates with our scoped token
    let scoped_thorium = scoped_client(&client, &scoped.token).await?;
    // make sure our scoped token works before it expires
    scoped_thorium.users.info().await?;
    // wait for our scoped token to permanently expire
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    // make sure our scoped token no longer works
    fail!(scoped_thorium.users.info().await, 401);
    // and is no longer listed
    let listed = client.users.list_scoped_tokens().await?;
    is!(listed.len(), 0);
    Ok(())
}

/// Unauthenticated self-registration must NOT grant the all-groups Analyst role without
/// the deployment secret key.
///
/// `POST /api/users/` is unauthenticated by design. An external attacker who can self-assign the
/// `Analyst` role gains read access to every group in the cluster (`db::users::get` expands an
/// Analyst's group list to all groups). The secure behavior is to reject a keyless `Analyst`
/// registration, so this asserts the request fails with `401`.
#[tokio::test]
async fn self_registration_analyst_requires_secret_key() -> Result<(), Error> {
    // get an admin client just to resolve the API host to talk to
    let client = test_utilities::admin_client().await?;
    // build a self-registration blueprint that self-assigns the all-groups Analyst role
    let blueprint = thorium::models::UserCreate::new(
        generators::gen_string(24),
        generators::gen_string(64),
        "attacker@evil.example",
    )
    .role(UserRole::Analyst);
    // use default client settings for this unauthenticated registration
    let settings = ClientSettings::default();
    // registering an Analyst without the secret key must be rejected
    test_utilities::assert_status(
        Users::create(&client.host, blueprint, None::<&str>, &settings).await,
        401,
        "keyless Analyst self-registration",
    )
}

/// Unauthenticated self-registration must NOT grant the developer role without
/// the deployment secret key.
///
/// `POST /api/users/` is unauthenticated by design. An external attacker who can self-assign the
/// `Developr` role gains the ability to create tools.
#[tokio::test]
async fn self_registration_developer_requires_secret_key() -> Result<(), Error> {
    // get an admin client just to resolve the API host to talk to
    let client = test_utilities::admin_client().await?;
    // build a self-registration blueprint that self-assigns the developr role
    let blueprint = thorium::models::UserCreate::new(
        generators::gen_string(24),
        generators::gen_string(64),
        "attacker@evil.example",
    )
    .role(UserRole::Developer {
        k8s: true,
        bare_metal: true,
        windows: true,
        external: true,
        kvm: true,
    });
    // use default client settings for this unauthenticated registration
    let settings = ClientSettings::default();
    // registering a developer without the secret key must be rejected
    test_utilities::assert_status(
        Users::create(&client.host, blueprint, None::<&str>, &settings).await,
        401,
        "keyless Developr self-registration",
    )
}

/// Client-controlled `skip_verification` must NOT be honored without the deployment secret
/// key.
///
/// `skip_verification` is documented as "requires secret key" but historically bypassed the email
/// verification gate for any unauthenticated registrant. The secure behavior is to reject a
/// keyless `skip_verification=true` registration, so this asserts the request fails with `401`.
#[tokio::test]
async fn skip_verification_requires_secret_key() -> Result<(), Error> {
    // get an admin client just to resolve the API host to talk to
    let client = test_utilities::admin_client().await?;
    // build a self-registration blueprint that tries to skip email verification without a key
    let blueprint = thorium::models::UserCreate::new(
        generators::gen_string(24),
        generators::gen_string(64),
        "attacker@evil.example",
    )
    .skip_verification();
    // use default client settings for this unauthenticated registration
    let settings = ClientSettings::default();
    // skipping verification without the secret key must be rejected
    test_utilities::assert_status(
        Users::create(&client.host, blueprint, None::<&str>, &settings).await,
        401,
        "keyless skip_verification self-registration",
    )
}

/// Build a lowercase scoped token name that is unique across test runs
fn token_name() -> String {
    // uuids in simple form are 32 lowercase hex characters
    Uuid::new_v4().simple().to_string()
}

/// Build a developer user role with the given scaler flags
///
/// # Arguments
///
/// * `k8s` - Whether this role can develop for K8s
/// * `bare_metal` - Whether this role can develop for bare metal
/// * `windows` - Whether this role can develop for Windows
fn dev_role(k8s: bool, bare_metal: bool, windows: bool) -> UserRole {
    UserRole::Developer {
        k8s,
        bare_metal,
        windows,
        external: false,
        kvm: false,
    }
}

/// Build a developer scoped token role with the given scaler flags
///
/// # Arguments
///
/// * `k8s` - Whether this token can develop for K8s
/// * `bare_metal` - Whether this token can develop for bare metal
/// * `windows` - Whether this token can develop for Windows
fn dev_token(k8s: bool, bare_metal: bool, windows: bool) -> ScopedTokenRole {
    ScopedTokenRole::Developer {
        k8s,
        bare_metal,
        windows,
        external: false,
        kvm: false,
    }
}

/// Create a scoped token for a user and get a client that authenticates with it
///
/// # Arguments
///
/// * `client` - The full token client of the user to create a scoped token for
/// * `role` - The role to give this scoped token
/// * `groups` - The groups to limit this scoped token to
async fn scoped_with_role(
    client: &Thorium,
    role: ScopedTokenRole,
    groups: Vec<&String>,
) -> Result<Thorium, Error> {
    // build a scoped token request with this role and these groups
    let req = ScopedTokenRequest::new(token_name())
        .role(role)
        .groups(groups);
    // create this scoped token
    let scoped = client.users.create_scoped_token(&req).await?;
    // build a client that authenticates with this scoped token
    scoped_client(client, &scoped.token).await
}

/// Upload a random sample to a single group and get its sha256 and submission id
///
/// # Arguments
///
/// * `group` - The group to upload this sample to
/// * `client` - The client to upload this sample with
async fn upload_sample(group: &str, client: &Thorium) -> Result<(String, Uuid), Error> {
    // build a sample with random data in this group
    let req = SampleRequest::new_buffer(
        thorium::models::Buffer::new(generators::gen_string(256)),
        vec![group],
    );
    // upload this sample
    let resp = client.files.create(req).await?;
    Ok((resp.sha256, resp.id))
}

/// Download a sample to a unique temporary path and remove it again
///
/// # Arguments
///
/// * `sha256` - The sha256 of the sample to download
/// * `client` - The client to download this sample with
async fn download_sample(sha256: &str, client: &Thorium) -> Result<(), Error> {
    // pick a unique download path so parallel tests do not collide
    let path = std::env::temp_dir().join(format!("scoped-role-{}", token_name()));
    // download this sample
    let mut opts = FileDownloadOpts::default();
    client.files.download(sha256, &path, &mut opts).await?;
    // remove the downloaded file
    tokio::fs::remove_file(&path).await?;
    Ok(())
}

/// A developer's `User` token acts as a `User` and fails developer checks
#[tokio::test]
async fn scoped_token_role_developer_user_token() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a K8s developer
    let dev = generators::user_with_role(&admin, dev_role(true, false, false)).await?;
    // create a group this developer owns
    let group = create_group(&dev.client).await?;
    // create a user scoped token for this group
    let scoped = scoped_with_role(&dev.client, ScopedTokenRole::User, vec![&group]).await?;
    // make sure the scoped token authenticates with the user role
    let info = scoped.users.info().await?;
    is!(info.role, UserRole::User);
    // build a pipeline whose image was created with the full token
    let pipe = generators::gen_pipe(&group, 1, false, &dev.client).await?;
    // a pipeline create needs a developer check so the scoped token is rejected
    fail!(scoped.pipelines.create(&pipe).await, 401);
    // and so is an image create
    let image = generators::gen_image(&group);
    test_utilities::faults::allow_image_writes(&group);
    fail!(scoped.images.create(&image).await, 401);
    // the full token still creates both
    dev.client.pipelines.create(&pipe).await?;
    dev.client.images.create(&image).await?;
    Ok(())
}

/// An analyst's `User` token acts as a `User` in exactly its groups
#[tokio::test]
async fn scoped_token_role_analyst_user_token() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create an analyst
    let analyst = generators::user_with_role(&admin, UserRole::Analyst).await?;
    // create a group the analyst isn't a direct member of
    let roles = generators::GroupRoles::default();
    let outside = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // create a group the analyst is a direct user of
    let roles = generators::GroupRoles::default().user(&analyst.username);
    let member = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // create a group the token won't hold
    let roles = generators::GroupRoles::default();
    let other = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // create a user scoped token for the first two groups
    let scoped = scoped_with_role(
        &analyst.client,
        ScopedTokenRole::User,
        vec![&outside, &member],
    )
    .await?;
    // make sure the token authenticates as a user with exactly its groups
    let info = scoped.users.info().await?;
    is!(info.role, UserRole::User);
    let mut groups = info.groups.clone();
    groups.sort();
    let mut expected = vec![outside.clone(), member.clone()];
    expected.sort();
    is!(groups, expected);
    // the analyst list still lets the token view and edit the group it isn't a member of
    scoped.groups.get(&outside).await?;
    let (sha256, _) = upload_sample(&outside, &scoped).await?;
    download_sample(&sha256, &scoped).await?;
    // a submission by someone else needs modify rights which the analyst list doesn't give
    let (admin_sha256, admin_submission) = upload_sample(&outside, &admin).await?;
    let opts = FileDeleteOpts::default().groups(vec![outside.clone()]);
    fail!(
        scoped
            .files
            .delete(&admin_sha256, &admin_submission, &opts)
            .await,
        401
    );
    // the token can't develop even in a group the analyst is a direct member of
    let image = generators::gen_image(&member);
    test_utilities::faults::allow_image_writes(&member);
    fail!(scoped.images.create(&image).await, 401);
    // while the analyst's full token can
    analyst.client.images.create(&image).await?;
    // a group outside the token is rejected
    fail!(scoped.groups.get(&other).await, 401);
    Ok(())
}

/// A scoped token's role is capped at its owner's current role
#[tokio::test]
async fn scoped_token_role_flags() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a developer for K8s, bare metal, and Windows
    let dev = generators::user_with_role(&admin, dev_role(true, true, true)).await?;
    // create a group this developer owns
    let group = create_group(&dev.client).await?;
    // create a K8s and Windows developer token
    let scoped = scoped_with_role(&dev.client, dev_token(true, false, true), vec![&group]).await?;
    // lower the developer to K8s and bare metal
    admin
        .users
        .update(&dev.username, role_update(dev_role(true, true, false)))
        .await?;
    // the token keeps only the flags both it and its owner have
    is!(
        scoped.users.info().await?.role,
        dev_role(true, false, false)
    );
    // lower the developer to bare metal only, which the token doesn't have
    admin
        .users
        .update(&dev.username, role_update(dev_role(false, true, false)))
        .await?;
    // with no flag left the developer token acts as a user
    is!(scoped.users.info().await?.role, UserRole::User);
    // lower the developer to the user role
    admin
        .users
        .update(&dev.username, role_update(UserRole::User))
        .await?;
    // the developer token now acts as a user
    is!(scoped.users.info().await?.role, UserRole::User);
    // disable the developer
    admin
        .users
        .update(&dev.username, role_update(UserRole::Disabled))
        .await?;
    // the token is rejected as disabled
    fail!(scoped.users.info().await, 401, "has been disabled");
    // an admin's and an analyst's K8s developer tokens keep only their own flags
    let analyst = generators::user_with_role(&admin, UserRole::Analyst).await?;
    let group = create_group(&analyst.client).await?;
    let scoped =
        scoped_with_role(&analyst.client, dev_token(true, false, false), vec![&group]).await?;
    is!(
        scoped.users.info().await?.role,
        dev_role(true, false, false)
    );
    let group = create_group(&admin).await?;
    let scoped = scoped_with_role(&admin, dev_token(true, false, false), vec![&group]).await?;
    is!(
        scoped.users.info().await?.role,
        dev_role(true, false, false)
    );
    Ok(())
}

/// An analyst can scope a token to a group they aren't a member of
#[tokio::test]
async fn scoped_token_analyst_any_group() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create an analyst
    let analyst = generators::user_with_role(&admin, UserRole::Analyst).await?;
    // create a group the analyst isn't a member of
    let roles = generators::GroupRoles::default();
    let group = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // upload a sample only to that group as the admin
    let (sha256, _) = upload_sample(&group, &admin).await?;
    // the analyst can create a scoped token for that group
    let scoped = scoped_with_role(&analyst.client, ScopedTokenRole::User, vec![&group]).await?;
    // the token authenticates as a user
    is!(scoped.users.info().await?.role, UserRole::User);
    // the token can download the sample
    download_sample(&sha256, &scoped).await?;
    // and tag it
    let tags = TagRequest::default().group(&group).add("scoped", "analyst");
    scoped.files.tag(&sha256, &tags).await?;
    Ok(())
}

/// The parts of a group a group update can write, in a comparable form
type GroupSnapshot = (Vec<BTreeSet<String>>, Option<String>, serde_json::Value);

/// Snapshot the direct members, description, and allowed flags of a group
///
/// # Arguments
///
/// * `group` - The group to snapshot
fn group_snapshot(group: &Group) -> GroupSnapshot {
    // sort a role's direct members so set order does not matter
    let sorted = |set: &HashSet<String>| set.iter().cloned().collect::<BTreeSet<String>>();
    // collect every role's direct members
    let roles = vec![
        sorted(&group.owners.direct),
        sorted(&group.managers.direct),
        sorted(&group.users.direct),
        sorted(&group.monitors.direct),
    ];
    // serialize the allowed flags since they have no equality check
    let allowed = serde_json::to_value(&group.allowed).expect("serialize allowed flags");
    (roles, group.description.clone(), allowed)
}

/// Build a group update that flips the files flag off
fn disable_files_update() -> GroupUpdate {
    GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_files(),
        ..GroupUpdate::default()
    }
}

/// An owner's scoped token can't write groups or read profiles, and an admin's scoped
/// token can't reach the admin dumps
#[tokio::test]
async fn scoped_token_full_token_routes() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create an owner, a co-owner, a member, and a user to add
    let owner = generators::user_with_role(&admin, UserRole::User).await?;
    let co_owner = generators::user_with_role(&admin, UserRole::User).await?;
    let member = generators::user_with_role(&admin, UserRole::User).await?;
    let other = generators::user_with_role(&admin, UserRole::User).await?;
    // create a group with both owners and the member
    let roles = generators::GroupRoles::default()
        .owner(&owner.username)
        .owner(&co_owner.username)
        .user(&member.username);
    let group = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // create scoped tokens for the owner and the member limited to this group
    let req = ScopedTokenRequest::new(token_name()).group(&group);
    let token = owner.client.users.create_scoped_token(&req).await?.token;
    let scoped = scoped_client(&owner.client, &token).await?;
    let member_scoped =
        scoped_with_role(&member.client, ScopedTokenRole::User, vec![&group]).await?;
    // snapshot the group before any scoped write
    let before = group_snapshot(&admin.groups.get(&group).await?);
    // a group create with the scoped token is rejected and creates nothing
    let new_group = Uuid::new_v4().simple().to_string();
    fail!(
        scoped.groups.create(&GroupRequest::new(&new_group)).await,
        401
    );
    fail!(admin.groups.get(&new_group).await, 404);
    // build the group updates the scoped tokens try
    let add_user =
        || GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&other.username));
    let remove_owner = || {
        GroupUpdate::default().owners(GroupUsersUpdate::default().direct_remove(&co_owner.username))
    };
    let remove_self = || {
        GroupUpdate::default().users(GroupUsersUpdate::default().direct_remove(&member.username))
    };
    // every group update with a scoped token is rejected
    fail!(scoped.groups.update(&group, &add_user()).await, 401);
    fail!(scoped.groups.update(&group, &remove_owner()).await, 401);
    fail!(
        scoped.groups.update(&group, &disable_files_update()).await,
        401
    );
    fail!(
        member_scoped.groups.update(&group, &remove_self()).await,
        401
    );
    // a group delete with the scoped token is rejected
    fail!(scoped.groups.delete(&group).await, 401);
    // nothing about the group was written
    is!(group_snapshot(&admin.groups.get(&group).await?), before);
    // reading the owner's own profile with the scoped token is rejected
    fail!(scoped.users.get(&owner.username).await, 401);
    // whoami with the scoped token returns the token's own value and narrowed groups
    let info = scoped.users.info().await?;
    is!(info.token, token);
    is!(info.groups, vec![group.clone()]);
    // and never the owner's primary token
    let body = serde_json::to_string(&info)?;
    is!(body.contains(&owner.token), false);
    // the full tokens can do everything the scoped tokens could not
    owner
        .client
        .groups
        .create(&GroupRequest::new(&new_group))
        .await?;
    owner.client.groups.update(&group, &add_user()).await?;
    owner.client.groups.update(&group, &remove_owner()).await?;
    owner
        .client
        .groups
        .update(&group, &disable_files_update())
        .await?;
    member.client.groups.update(&group, &remove_self()).await?;
    is!(
        owner.client.users.get(&owner.username).await?.username,
        owner.username.clone()
    );
    // check every update landed
    let after = admin.groups.get(&group).await?;
    is!(after.users.direct.contains(&other.username), true);
    is!(after.users.direct.contains(&member.username), false);
    is!(after.owners.direct.contains(&co_owner.username), false);
    is!(after.allowed.files, false);
    // the full token can delete the group
    owner.client.groups.delete(&group).await?;
    fail!(admin.groups.get(&group).await, 404);
    Ok(())
}

/// An admin's scoped token can't reach the admin dumps
#[tokio::test]
async fn scoped_token_admin_dumps() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // create a scoped token for the admin limited to one group
    let group = create_group(&admin).await?;
    let scoped = scoped_with_role(&admin, ScopedTokenRole::User, vec![&group]).await?;
    // the admin's full token reaches the backup
    let backup = admin.system.backup().await?;
    // the scoped token is rejected on the backup and the restore
    fail!(scoped.system.backup().await, 401);
    fail!(scoped.system.restore(&backup).await, 401);
    // the scoped token is rejected on reactions created as other users
    let reqs = HashMap::new();
    fail!(scoped.reactions.create_bulk_by_user(&reqs).await, 401);
    // the admin's full token reaches it
    admin.reactions.create_bulk_by_user(&reqs).await?;
    // the scoped token is rejected on every user's details
    fail!(scoped.users.list_details().await, 401);
    // the admin's full token reaches them
    admin.users.list_details().await?;
    Ok(())
}

/// Generate a unique model secret value
fn gen_model_secret() -> String {
    format!("sk-{}", generators::gen_string(32).to_lowercase())
}

/// Build an `OpenAiCompatible` provider with a unique url
fn unique_compatible() -> ModelProvider {
    ModelProvider::OpenAiCompatible {
        base_url: format!(
            "https://{}.compat.local/v1",
            generators::gen_string(12).to_lowercase()
        ),
        extra_headers: BTreeMap::new(),
        key_header: None,
    }
}

/// Build one of a user's own endpoints that the deployment allows
///
/// # Arguments
///
/// * `name` - The endpoint's name
/// * `model_id` - A model in its catalog
async fn own_endpoint(name: &str, model_id: &str) -> Result<UserModelEndpoint, Error> {
    // a unique gateway the deployment allows
    let provider = unique_compatible();
    generators::allow_model_endpoint(&provider).await?;
    Ok(UserModelEndpoint::new(name, provider).model(ModelCatalogEntryRequest::new(model_id)))
}

/// Send an update of a user's own model settings
///
/// # Arguments
///
/// * `client` - The client of the user to update
/// * `update` - The model settings update
async fn set_models(client: &Thorium, update: UserModelSettingsUpdate) -> Result<(), Error> {
    // wrap the model update in a user settings update
    let ai = AiSettingsUpdate::default().models(update);
    let settings = UserSettingsUpdate::default().ai(ai);
    client
        .users
        .update_self(UserUpdate::default().settings(settings))
        .await?;
    Ok(())
}

/// Get a user's own model settings
///
/// # Arguments
///
/// * `client` - The client of the user to read
async fn models_of(client: &Thorium) -> Result<UserModelSettings, Error> {
    // the settings come back on whoami
    let info = client.users.info().await?;
    Ok(info.settings.ai.map_or_default(|ai| ai.models))
}

/// Get a raw API response body with an authorization header
///
/// # Arguments
///
/// * `auth` - The authorization header to send
/// * `path` - The API path, starting with `/api/`
async fn raw_body(auth: &str, path: &str) -> Result<String, Error> {
    // send the request with the given header
    let admin = test_utilities::admin_client().await?;
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", admin.host))
        .header("authorization", auth)
        .send()
        .await?;
    // only a successful response is inspected
    assert!(
        resp.status().is_success(),
        "GET {path} returned {}",
        resp.status()
    );
    Ok(resp.text().await?)
}

/// Build the authorization header for a raw token
///
/// # Arguments
///
/// * `token` - The token to send
fn token_header(token: &str) -> String {
    // base64 encode the token like the client does
    format!(
        "token {}",
        base64::engine::general_purpose::STANDARD.encode(token.as_bytes())
    )
}

/// Make sure a body holds none of some secrets without ever printing it
///
/// # Arguments
///
/// * `body` - The response body
/// * `secrets` - The secrets that must not appear
/// * `what` - What the body is, for the failure message
fn assert_no_secret(body: &str, secrets: &[&str], what: &str) {
    // check each secret without printing anything that might hold one
    for (index, secret) in secrets.iter().enumerate() {
        assert!(!body.contains(secret), "{what} returned secret #{index}");
    }
}

/// Read the stored settings of a user straight from Redis
///
/// # Arguments
///
/// * `username` - The user to read
async fn stored_settings(username: &str) -> Result<UserSettings, Error> {
    // connect to the Redis instance the test API uses
    let redis = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(format!("Failed to get a Redis connection: {err}")))?;
    // read the settings field of the user's data hash
    let key = format!(
        "{}:user_data:{username}",
        test_utilities::CONF.thorium.namespace
    );
    let raw: String = redis::cmd("hget")
        .arg(&key)
        .arg("settings")
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("Failed to read user settings: {err}")))?;
    // the field holds only the AI settings, so the theme reads as the default
    let mut stored: serde_json::Value = serde_json::from_str(&raw)?;
    if let Some(fields) = stored.as_object_mut() {
        fields
            .entry("theme")
            .or_insert_with(|| serde_json::json!(UserSettings::default().theme));
    }
    Ok(serde_json::from_value(stored)?)
}

/// A user stores their own endpoints, credentials, and default profiles, unknown or invisible
/// references are refused, and every response is redacted
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn user_model_settings() -> Result<(), Error> {
    // a user in a group with a keyless group endpoint and a profile, and a group they can't see
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    let group = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let hidden =
        generators::group_with_roles(&GroupRoles::default(), GroupAllowed::default(), &admin)
            .await?;
    let provider = unique_compatible();
    let entry = ModelCatalogEntryRequest::new("served");
    let group_endpoint = generators::model_endpoint(
        &[&group],
        ModelEndpointEntityRequest::new(provider).model(entry.clone()),
        &admin,
    )
    .await?;
    let hidden_endpoint = generators::model_endpoint(
        &[&hidden],
        ModelEndpointEntityRequest::new(unique_compatible()).model(entry),
        &admin,
    )
    .await?;
    let profile = generators::model_profile(
        &[&group],
        ModelProfileEntityRequest::new(group_endpoint.id, "served", ModelApi::OpenAiChat),
        &admin,
    )
    .await?;
    let hidden_profile = generators::model_profile(
        &[&hidden],
        ModelProfileEntityRequest::new(hidden_endpoint.id, "served", ModelApi::OpenAiChat),
        &admin,
    )
    .await?;
    // a user with no model settings reads empty ones
    is!(models_of(&user.client).await?, UserModelSettings::default());
    // the user adds an endpoint, credentials for it and the group endpoint, and a default profile
    let own_id = Uuid::new_v4();
    let own = own_endpoint("mine", "own-model").await?;
    let own_secret = gen_model_secret();
    let group_secret = gen_model_secret();
    let update = UserModelSettingsUpdate::default()
        .endpoint(own_id, own)
        .credential(
            own_id,
            UserCredentialRef::new(CredentialKind::ApiKey, Some(own_secret.as_str().into())),
        )
        .credential(
            group_endpoint.id,
            UserCredentialRef::new(CredentialKind::ApiKey, Some(group_secret.as_str().into())),
        )
        .default_profiles(vec![profile.id]);
    set_models(&user.client, update).await?;
    // the settings come back with fingerprints and no secrets
    let models = models_of(&user.client).await?;
    is!(models.endpoints.len(), 1);
    is!(models.endpoints[&own_id].name.as_str(), "mine");
    is!(models.default_profiles.clone(), vec![profile.id]);
    for (endpoint, secret) in [(own_id, &own_secret), (group_endpoint.id, &group_secret)] {
        let cred = &models.credentials[&endpoint];
        is!(cred.secrets.primary.clone(), None::<ModelSecret>);
        is!(
            cred.secrets.fingerprint.clone(),
            Some(ModelSecret::from(secret.as_str()).fingerprint())
        );
    }
    // no user route returns either secret
    let admin_user = generators::user_with_role(&admin, UserRole::Admin).await?;
    let secrets = [own_secret.as_str(), group_secret.as_str()];
    for path in [
        "/api/users/whoami".to_owned(),
        format!("/api/users/user/{}", user.username),
    ] {
        let body = raw_body(&user.auth_header(), &path).await?;
        assert_no_secret(&body, &secrets, &path);
        assert!(body.contains(&ModelSecret::from(own_secret.as_str()).fingerprint()));
    }
    let body = raw_body(&admin_user.auth_header(), "/api/users/details/").await?;
    assert_no_secret(&body, &secrets, "the user details list");
    // the secrets are stored
    let stored = stored_settings(&user.username).await?;
    let stored = stored
        .ai
        .expect("the stored settings have no ai settings")
        .models;
    assert!(
        stored.credentials[&own_id]
            .secrets
            .primary
            .as_ref()
            .is_some_and(|secret| secret.expose() == own_secret),
        "the user's own secret wasn't stored"
    );
    // an unknown or invisible endpoint for a credential is refused
    for endpoint in [Uuid::new_v4(), hidden_endpoint.id] {
        let update = UserModelSettingsUpdate::default().credential(
            endpoint,
            UserCredentialRef::new(CredentialKind::ApiKey, Some(gen_model_secret().into())),
        );
        let resp = set_models(&user.client, update).await;
        fail!(resp, 400);
    }
    // an unknown or invisible default profile is refused
    for profile in [Uuid::new_v4(), hidden_profile.id] {
        let update = UserModelSettingsUpdate::default().default_profiles(vec![profile]);
        let resp = set_models(&user.client, update).await;
        fail!(resp, 400);
    }
    // a credential kind the endpoint's provider doesn't accept is refused
    let update = UserModelSettingsUpdate::default().credential(
        own_id,
        UserCredentialRef::new(
            CredentialKind::AwsBearerToken,
            Some(gen_model_secret().into()),
        ),
    );
    let resp = set_models(&user.client, update).await;
    fail!(resp, 400);
    // an empty secret counts as no secret
    let update = UserModelSettingsUpdate::default().credential(
        Uuid::new_v4(),
        UserCredentialRef::new(CredentialKind::ApiKey, Some("".into())),
    );
    let resp = set_models(&user.client, update).await;
    fail!(resp, 400);
    // nothing a refused update held was stored
    is!(models_of(&user.client).await?, models);
    // removing the own endpoint also removes its credential
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().remove_endpoint(own_id),
    )
    .await?;
    let models = models_of(&user.client).await?;
    assert!(models.endpoints.is_empty(), "the endpoint wasn't removed");
    assert!(
        !models.credentials.contains_key(&own_id),
        "the endpoint's credential was left behind"
    );
    assert!(models.credentials.contains_key(&group_endpoint.id));
    // removing the other credential and clearing the defaults leaves empty settings
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .remove_credential(group_endpoint.id)
            .default_profiles(Vec::new()),
    )
    .await?;
    is!(models_of(&user.client).await?, UserModelSettings::default());
    Ok(())
}

/// User credentials keep their secret across updates without one, rotate with a new one, and
/// assumed roles must name another of the same user's access keys
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn user_model_credentials() -> Result<(), Error> {
    // a user with a Bedrock endpoint of their own
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    let provider = ModelProvider::Bedrock {
        region: "us-east-1".into(),
        endpoint_kind: BedrockEndpointKind::Runtime,
        endpoint_url: None,
        workspace_id: None,
    };
    generators::allow_model_endpoint(&provider).await?;
    let endpoint = Uuid::new_v4();
    let second = Uuid::new_v4();
    let secret = gen_model_secret();
    let access = UserCredentialRef::new(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIA".into(),
        },
        Some(secret.as_str().into()),
    );
    let access_id = access.id;
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .endpoint(
                endpoint,
                UserModelEndpoint::new("bedrock", provider.clone()),
            )
            .endpoint(
                second,
                UserModelEndpoint::new("bedrock-2", provider.clone()),
            )
            .credential(endpoint, access.clone()),
    )
    .await?;
    let first = models_of(&user.client).await?.credentials[&endpoint].clone();
    is!(first.id, access_id);
    let fingerprint = ModelSecret::from(secret.as_str()).fingerprint();
    is!(first.secrets.fingerprint.clone(), Some(fingerprint.clone()));
    // an update of the credential without a secret keeps the stored one
    let mut keep = access.clone();
    keep.secrets.primary = None;
    keep.expires = Some(Utc::now() + Duration::days(30));
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().credential(endpoint, keep),
    )
    .await?;
    let kept = models_of(&user.client).await?.credentials[&endpoint].clone();
    is!(kept.secrets.fingerprint.clone(), Some(fingerprint.clone()));
    is!(kept.rotated, first.rotated);
    let stored = stored_settings(&user.username).await?;
    assert!(
        stored.ai.expect("no stored ai settings").models.credentials[&endpoint]
            .secrets
            .primary
            .as_ref()
            .is_some_and(|stored| stored.expose() == secret),
        "the stored secret changed"
    );
    // a new secret rotates the credential
    let new_secret = gen_model_secret();
    let mut rotate = access.clone();
    rotate.secrets.primary = Some(new_secret.as_str().into());
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().credential(endpoint, rotate),
    )
    .await?;
    let rotated = models_of(&user.client).await?.credentials[&endpoint].clone();
    is!(
        rotated.secrets.fingerprint.clone(),
        Some(ModelSecret::from(new_secret.as_str()).fingerprint())
    );
    assert!(
        rotated.rotated > first.rotated,
        "rotation didn't update rotated"
    );
    // an assumed role naming the user's access key is accepted
    let role = |source| {
        UserCredentialRef::new(
            CredentialKind::AwsAssumeRole {
                role_arn: "arn:aws:iam::1:role/r".into(),
                external_id: None,
                session_name: None,
                duration_seconds: None,
                source: AwsRoleSource::AccessKey(source),
            },
            None,
        )
    };
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().credential(second, role(access_id)),
    )
    .await?;
    // one naming a missing credential or a credential that isn't an access key is refused
    let api_key_endpoint = Uuid::new_v4();
    let compat = unique_compatible();
    generators::allow_model_endpoint(&compat).await?;
    let api_key = UserCredentialRef::new(CredentialKind::ApiKey, Some(gen_model_secret().into()));
    let api_key_id = api_key.id;
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .endpoint(api_key_endpoint, UserModelEndpoint::new("compat", compat))
            .credential(api_key_endpoint, api_key),
    )
    .await?;
    for source in [Uuid::new_v4(), api_key_id] {
        let update = UserModelSettingsUpdate::default().credential(second, role(source));
        let resp = set_models(&user.client, update).await;
        fail!(resp, 400);
    }
    Ok(())
}

/// A user adds and clears Generic bans on their own endpoint, catalog model, and credential,
/// other ban kinds are refused, and each ban is a notification only that user reads and dismisses
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn user_model_bans_and_notifications() -> Result<(), Error> {
    // a user with an endpoint and a credential of their own
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    let other = generators::user_with_role(&admin, UserRole::User).await?;
    let endpoint = Uuid::new_v4();
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .endpoint(endpoint, own_endpoint("banned", "model-a").await?)
            .credential(
                endpoint,
                UserCredentialRef::new(CredentialKind::ApiKey, Some(gen_model_secret().into())),
            ),
    )
    .await?;
    // the user bans their endpoint, its catalog model, and its credential by hand
    let endpoint_ban = ModelBan::generic("endpoint paused");
    let model_ban = ModelBan::generic("model paused");
    let cred_ban = ModelBan::generic("credential paused");
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .bans(
                endpoint,
                ModelBanUpdate::default().add_ban(endpoint_ban.clone()),
            )
            .model_bans(
                endpoint,
                "model-a",
                ModelBanUpdate::default().add_ban(model_ban.clone()),
            )
            .credential_bans(
                endpoint,
                ModelBanUpdate::default().add_ban(cred_ban.clone()),
            ),
    )
    .await?;
    let models = models_of(&user.client).await?;
    assert!(
        models.endpoints[&endpoint]
            .bans
            .contains_key(&endpoint_ban.id)
    );
    assert!(
        models.endpoints[&endpoint].catalog[0]
            .bans
            .contains_key(&model_ban.id)
    );
    assert!(
        models.credentials[&endpoint]
            .bans
            .contains_key(&cred_ban.id)
    );
    // each ban is one notification for the user, and the other user has none of them
    let notes = user.client.users.notifications().await?;
    let banned = |id: Uuid| notes.iter().filter(|note| note.ban_id == Some(id)).count();
    is!(banned(endpoint_ban.id), 1);
    is!(banned(model_ban.id), 1);
    is!(banned(cred_ban.id), 1);
    let others = other.client.users.notifications().await?;
    assert!(
        others.iter().all(|note| note.key == other.username),
        "a user read another user's notification"
    );
    // ban ids are unique across the endpoint, its catalog, and its credential
    let reused = ModelBan {
        id: endpoint_ban.id,
        ..ModelBan::generic("again")
    };
    let update = UserModelSettingsUpdate::default()
        .credential_bans(endpoint, ModelBanUpdate::default().add_ban(reused));
    let resp = set_models(&user.client, update).await;
    fail!(resp, 400);
    // any other ban kind is refused, even for the user's own items
    for kind in [
        ModelBanKind::NotAllowedByDeployment,
        ModelBanKind::CredentialExpired,
        ModelBanKind::BrokenReference(Uuid::new_v4()),
    ] {
        let update = UserModelSettingsUpdate::default().bans(
            endpoint,
            ModelBanUpdate::default().add_ban(ModelBan::new(kind)),
        );
        let refusal = set_models(&user.client, update).await;
        fail!(refusal, 400);
    }
    // another user can't dismiss the user's notification and it stays
    let note = notes
        .iter()
        .find(|note| note.ban_id == Some(cred_ban.id))
        .expect("no notification for the credential ban");
    let resp = other.client.users.delete_notification(note.id).await;
    fail!(resp, 404);
    is!(
        user.client
            .users
            .notifications()
            .await?
            .iter()
            .filter(|kept| kept.id == note.id)
            .count(),
        1
    );
    // the user clears the bans by id and their notifications go away
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().bans(
            endpoint,
            ModelBanUpdate::default()
                .remove_ban(endpoint_ban.id)
                .remove_ban(model_ban.id)
                .remove_ban(cred_ban.id),
        ),
    )
    .await?;
    let models = models_of(&user.client).await?;
    assert!(models.endpoints[&endpoint].bans.is_empty());
    assert!(models.endpoints[&endpoint].catalog[0].bans.is_empty());
    assert!(models.credentials[&endpoint].bans.is_empty());
    let notes = user.client.users.notifications().await?;
    assert!(
        notes.iter().all(|note| note.ban_id.is_none()),
        "a cleared ban left its notification"
    );
    // removing a ban that doesn't exist is a 404
    let update = UserModelSettingsUpdate::default().bans(
        endpoint,
        ModelBanUpdate::default().remove_ban(Uuid::new_v4()),
    );
    let resp = set_models(&user.client, update).await;
    fail!(resp, 404);
    // the user dismisses a notification of their own
    let ban = ModelBan::generic("dismiss me");
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .bans(endpoint, ModelBanUpdate::default().add_ban(ban.clone())),
    )
    .await?;
    let note = user
        .client
        .users
        .notifications()
        .await?
        .into_iter()
        .find(|note| note.ban_id == Some(ban.id))
        .expect("no notification for the new ban");
    user.client.users.delete_notification(note.id).await?;
    assert!(
        user.client
            .users
            .notifications()
            .await?
            .iter()
            .all(|kept| kept.id != note.id),
        "the dismissed notification stayed"
    );
    Ok(())
}

/// A user's own endpoint disables and enables model ids
#[tokio::test]
async fn user_disabled_models() -> Result<(), Error> {
    // a user with an endpoint of their own
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    let endpoint = Uuid::new_v4();
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().endpoint(endpoint, own_endpoint("e", "m").await?),
    )
    .await?;
    // disabling ids adds them
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .disable_model(endpoint, "a")
            .disable_model(endpoint, "b"),
    )
    .await?;
    let disabled = models_of(&user.client).await?.endpoints[&endpoint]
        .disabled_models
        .clone();
    is!(disabled, BTreeSet::from(["a".to_owned(), "b".to_owned()]));
    // enabling one removes it
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().enable_model(endpoint, "a"),
    )
    .await?;
    let disabled = models_of(&user.client).await?.endpoints[&endpoint]
        .disabled_models
        .clone();
    is!(disabled, BTreeSet::from(["b".to_owned()]));
    // an endpoint that isn't the user's own can't disable models
    let update = UserModelSettingsUpdate::default().disable_model(Uuid::new_v4(), "a");
    let resp = set_models(&user.client, update).await;
    fail!(resp, 400);
    Ok(())
}

/// A user's own endpoint must be on the deployment's allowed list
#[tokio::test]
async fn user_endpoints_follow_the_whitelist() -> Result<(), Error> {
    // a user and a gateway the deployment doesn't allow
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    let provider = unique_compatible();
    let endpoint = Uuid::new_v4();
    let update = UserModelSettingsUpdate::default()
        .endpoint(endpoint, UserModelEndpoint::new("gw", provider.clone()));
    let refusal = set_models(&user.client, update.clone()).await;
    fail!(refusal, 400);
    let msg = refusal.err().and_then(|err| err.msg()).unwrap_or_default();
    assert!(
        msg.contains("gw"),
        "the refusal didn't name the endpoint: {msg}"
    );
    // once allowed the same endpoint is accepted, and one trailing slash still matches
    generators::allow_model_endpoint(&provider).await?;
    set_models(&user.client, update).await?;
    let ModelProvider::OpenAiCompatible { base_url, .. } = &provider else {
        unreachable!("the provider is OpenAiCompatible");
    };
    let slashed = ModelProvider::OpenAiCompatible {
        base_url: format!("{base_url}/"),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .endpoint(Uuid::new_v4(), UserModelEndpoint::new("slash", slashed)),
    )
    .await?;
    // a url that differs in any other way is refused
    for url in [
        format!("{base_url}//"),
        base_url.replacen("https://", "https://X", 1),
        base_url.replace("/v1", "/v2"),
    ] {
        let changed = ModelProvider::OpenAiCompatible {
            base_url: url,
            extra_headers: BTreeMap::new(),
            key_header: None,
        };
        let update = UserModelSettingsUpdate::default()
            .endpoint(Uuid::new_v4(), UserModelEndpoint::new("changed", changed));
        let resp = set_models(&user.client, update).await;
        fail!(resp, 400);
    }
    Ok(())
}

/// Legacy AI endpoints are stored with their keys but every user response returns the key empty
#[tokio::test]
async fn legacy_ai_keys_are_never_returned() -> Result<(), Error> {
    // a user sets a legacy AI endpoint with a key
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    let key = gen_model_secret();
    let ai = AiSettingsUpdate::default()
        .endpoint(
            "main",
            AiEndpointUpdate::default()
                .url("https://llm.local")
                .api_key(&key)
                .model("m"),
        )
        .default_endpoint("main");
    user.client
        .users
        .update_self(UserUpdate::default().settings(UserSettingsUpdate::default().ai(ai)))
        .await?;
    // the key is stored
    let stored = stored_settings(&user.username).await?;
    let stored_key = stored.ai.expect("no stored ai settings").endpoints["main"]
        .api_key
        .clone();
    assert!(stored_key == key, "the legacy key wasn't stored");
    // every user response returns the url and model with an empty key
    let info = user.client.users.info().await?;
    let legacy = &info.settings.ai.expect("no ai settings").endpoints["main"];
    is!(legacy.url.as_str(), "https://llm.local");
    is!(legacy.model.as_str(), "m");
    assert!(legacy.api_key.is_empty(), "whoami returned a legacy key");
    let admin_user = generators::user_with_role(&admin, UserRole::Admin).await?;
    for (auth, path) in [
        (user.auth_header(), "/api/users/whoami".to_owned()),
        (
            admin_user.auth_header(),
            format!("/api/users/user/{}", user.username),
        ),
        (admin_user.auth_header(), "/api/users/details/".to_owned()),
    ] {
        let body = raw_body(&auth, &path).await?;
        assert_no_secret(&body, &[&key], &path);
    }
    // a later update of only the model settings leaves the stored legacy key alone
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().endpoint(Uuid::new_v4(), own_endpoint("e", "m").await?),
    )
    .await?;
    let after = stored_settings(&user.username).await?;
    let after_key = after.ai.expect("no stored ai settings").endpoints["main"]
        .api_key
        .clone();
    assert!(
        after_key == key,
        "a model update changed the stored legacy key"
    );
    Ok(())
}

/// A model settings update with no legacy endpoints creates the AI settings
#[tokio::test]
async fn model_settings_create_ai_settings() -> Result<(), Error> {
    // a user with no AI settings
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    is!(
        user.client.users.info().await?.settings.ai,
        None::<AiSettings>
    );
    // an update of only the model settings creates AI settings with no legacy endpoints
    let endpoint = Uuid::new_v4();
    set_models(
        &user.client,
        UserModelSettingsUpdate::default().endpoint(endpoint, own_endpoint("e", "m").await?),
    )
    .await?;
    let ai = user
        .client
        .users
        .info()
        .await?
        .settings
        .ai
        .expect("the model update created no ai settings");
    assert!(ai.endpoints.is_empty());
    is!(ai.default_endpoint.as_str(), "");
    assert!(ai.models.endpoints.contains_key(&endpoint));
    Ok(())
}

/// A scoped token's whoami returns only fingerprints and empty legacy keys
#[tokio::test]
async fn scoped_whoami_holds_no_model_secrets() -> Result<(), Error> {
    // a user sets a credential and a legacy key
    let admin = test_utilities::admin_client().await?;
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    let group = create_group(&user.client).await?;
    let secret = gen_model_secret();
    let legacy = gen_model_secret();
    let endpoint = Uuid::new_v4();
    set_models(
        &user.client,
        UserModelSettingsUpdate::default()
            .endpoint(endpoint, own_endpoint("e", "m").await?)
            .credential(
                endpoint,
                UserCredentialRef::new(CredentialKind::ApiKey, Some(secret.as_str().into())),
            ),
    )
    .await?;
    let ai = AiSettingsUpdate::default()
        .endpoint(
            "main",
            AiEndpointUpdate::default()
                .url("https://llm.local")
                .api_key(&legacy)
                .model("m"),
        )
        .default_endpoint("main");
    user.client
        .users
        .update_self(UserUpdate::default().settings(UserSettingsUpdate::default().ai(ai)))
        .await?;
    // the user's scoped token reads whoami
    let req = ScopedTokenRequest::new(token_name())
        .role(ScopedTokenRole::User)
        .groups(vec![&group]);
    let scoped = user.client.users.create_scoped_token(&req).await?;
    let body = raw_body(&token_header(&scoped.token), "/api/users/whoami").await?;
    assert_no_secret(&body, &[&secret, &legacy], "a scoped whoami");
    assert!(
        body.contains(&ModelSecret::from(secret.as_str()).fingerprint()),
        "the scoped whoami lost the fingerprint"
    );
    let info: ScrubbedUser = serde_json::from_str(&body)?;
    let ai = info.settings.ai.expect("no ai settings");
    assert!(
        ai.endpoints["main"].api_key.is_empty(),
        "a scoped whoami returned a legacy key"
    );
    is!(
        ai.models.credentials[&endpoint].secrets.primary.clone(),
        None::<ModelSecret>
    );
    Ok(())
}

/// Deleting a user deletes their notifications, so a new user with the same name has none
#[tokio::test]
async fn user_delete_removes_notifications() -> Result<(), Error> {
    // pick the name, password, and settings of a plain user
    let admin = test_utilities::admin_client().await?;
    let username = generators::gen_string(24);
    let password = generators::gen_string(64);
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    let settings = ClientSettings::default();
    let create = || {
        UserCreate::new(&username, &password, "fake@fake.gov")
            .skip_verification()
            .role(UserRole::User)
    };
    // create the user and log in as them
    Users::create(&admin.host, create(), secret_key, &settings).await?;
    let (token, _) =
        Thorium::auth(&admin.host, &username, &password, &reqwest::Client::new()).await?;
    let client = Thorium::build(&admin.host).token(&token).build().await?;
    // give them a banned endpoint of their own, which is one notification
    let endpoint = Uuid::new_v4();
    set_models(
        &client,
        UserModelSettingsUpdate::default()
            .endpoint(endpoint, own_endpoint("banned", "model-a").await?)
            .bans(
                endpoint,
                ModelBanUpdate::default().add_ban(ModelBan::generic("paused")),
            ),
    )
    .await?;
    is!(client.users.notifications().await?.len(), 1);
    // deleting the user deletes the notification
    admin.users.delete(&username).await?;
    is!(
        test_utilities::notification_rows(NotificationType::Users, &username).await?,
        0
    );
    // create a new user with the same name and log in as them
    Users::create(&admin.host, create(), secret_key, &settings).await?;
    let (token, _) =
        Thorium::auth(&admin.host, &username, &password, &reqwest::Client::new()).await?;
    let client = Thorium::build(&admin.host).token(&token).build().await?;
    // the new user has no notifications
    is!(client.users.notifications().await?.len(), 0);
    Ok(())
}
