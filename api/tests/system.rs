//! Tests the system routes in Thorium

use http::StatusCode;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use base64::Engine as _;
use thorium::models::{
    Backup, GroupAllowedUpdate, GroupUpdate, HostPathWhitelistUpdate, ImageBan, ImageBanKind,
    ImageBanUpdate, ImageNetworkPolicyUpdate, ImageScaler, ImageUpdate, NetworkPolicyUpdate,
    PipelineBan, PipelineBanKind, PipelineBanUpdate, PipelineRequest, PipelineUpdate, Pools,
    RepoRequest, Resources, ScopedTokenRequest, SystemInfo, SystemSettings,
    SystemSettingsResetParams, SystemSettingsUpdate, SystemSettingsUpdateParams, Volume,
    VolumeTypes, WorkerDeleteMap, WorkerRegistrationList,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, contains, fail, is, is_not, unwrap_variant, vec_in_vec};

#[serial_test::serial]
#[tokio::test]
async fn backup() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // reset settings to defaults
    client
        .system
        .reset_settings(&SystemSettingsResetParams::default().no_scan())
        .await?;
    // generate our groups/images/pipelines
    let (groups, images, pipes) = generators::gen_all(3, &client).await?;
    // get the backup
    let backup = client.system.backup().await?;
    // make sure the backup contains the default system settings that were set
    is!(&backup.settings, &SystemSettings::default());
    // make sure that our users list is not empty
    is!(backup.users.is_empty(), false);
    // make sure the backup contains all the groups/images/pipelines we created
    vec_in_vec!(&groups, &backup.groups);
    vec_in_vec!(&images, &backup.images);
    vec_in_vec!(&pipes, &backup.pipelines);
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn restore() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // generate our groups/images/pipelines
    let (groups, images, pipes) = generators::gen_all(3, &client).await?;
    // get our user
    let user = client.users.get("thorium").await?;
    // get the backup
    let backup = client.system.backup().await?;
    // restore our backup
    client.system.restore(&backup).await?;
    // get our user post restore
    let restored_user = client.users.get("thorium").await?;
    // make sure the user is correct
    is!(&user, &restored_user);
    // list the groups we just restored
    let resp = client.groups.list().page_size(500).details().exec().await?;
    // make sure all the group details we tried to restored are in our list
    vec_in_vec!(&groups, &resp.details);
    // crawl over the groups we restored and check to make sure all our images/pipelines were restored
    let mut restored_images = vec![];
    let mut restored_pipelines = vec![];
    for group in &groups {
        // list the images we just restored for this group
        let resp = client
            .images
            .list(&group.name)
            .page_size(500)
            .details()
            .exec()
            .await?;
        // add these images to our restored images list
        restored_images.extend(resp.details);
        // list the pipelines we just restored for this group
        let resp = client
            .pipelines
            .list(&group.name)
            .page_size(500)
            .details()
            .exec()
            .await?;
        // add these pipelines to our restored pipelines list
        restored_pipelines.extend(resp.details);
    }
    // make sure all the image/pipeline details we tried to restored are in our lists
    vec_in_vec!(&images, &restored_images);
    vec_in_vec!(&pipes, &restored_pipelines);
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn restore_create_reactions() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // generate our groups/images/pipelines
    let (groups, _, _) = generators::gen_all(3, &client).await?;
    // get the backup
    let backup = client.system.backup().await?;
    // restore our backup
    client.system.restore(&backup).await?;
    // try to create a reaction for each pipeline to make sure the restore worked
    for group in &groups {
        // list the pipelines we just restored for this group
        let resp = client
            .pipelines
            .list(&group.name)
            .page_size(500)
            .details()
            .exec()
            .await?;
        for pipe in &resp.details {
            // Create a random reaction based on our pipeline request
            let react_req = generators::gen_reaction(&group.name, pipe, None);
            client.reactions.create(&react_req).await?;
        }
    }
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn update_settings() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let whitelist = ["/mnt/valid_path", "/mnt/valid_path2"];
    // create a new settings update
    let update = SystemSettingsUpdate::default()
        .reserved_cpu("10")
        .reserved_memory("1G")
        .reserved_storage("1G")
        .fairshare_cpu("5")
        .fairshare_memory("512M")
        .fairshare_storage("512M")
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_paths(whitelist))
        .allow_unrestricted_host_paths(true);
    // build our params
    let default_params = SystemSettingsUpdateParams::default();
    // Update the settings
    client
        .system
        .update_settings(&update, &default_params)
        .await?;
    // Check that the settings were properly updated
    let settings = client.system.get_settings().await?;
    is!(&settings, &update);
    // update the settings again, removing one of the paths
    let update = SystemSettingsUpdate::default()
        .host_path_whitelist(HostPathWhitelistUpdate::default().remove_path(whitelist[0]));
    client
        .system
        .update_settings(&update, &default_params)
        .await?;
    // Check that the settings were properly updated
    let settings = client.system.get_settings().await?;
    is!(&settings, &update);
    // attempt to update settings as a regular user
    let user_client = generators::client(&client).await?;
    let resp = user_client
        .system
        .update_settings(&update, &default_params)
        .await;
    fail!(resp, 401);
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn reset_settings() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let initially_valid_path = "/mnt/valid_path";
    // create a new settings update
    let update = SystemSettingsUpdate::default()
        .reserved_cpu("10")
        .reserved_memory("1G")
        .reserved_storage("1G")
        .fairshare_cpu("5")
        .fairshare_memory("512M")
        .fairshare_storage("512M")
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_path(initially_valid_path))
        .allow_unrestricted_host_paths(true);
    // Update the settings
    client
        .system
        .update_settings(&update, &SystemSettingsUpdateParams::default().no_scan())
        .await?;
    // Check that the settings were properly updated
    let settings = client.system.get_settings().await?;
    is!(&settings, &update);
    // create user client
    let user_client = generators::client(&client).await?;
    // create group
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    // create an image with a whitelisted host path
    let image_req = generators::gen_host_path(&group, initially_valid_path);
    user_client.images.create(&image_req).await?;
    // reset system settings, performing an automatic scan
    client
        .system
        .reset_settings(&SystemSettingsResetParams::default())
        .await?;
    // ensure settings have been reset
    let settings = client.system.get_settings().await?;
    is!(&settings, &SystemSettings::default());
    // make sure that image has a ban with that path now that it's no longer whitelisted
    let image = user_client.images.get(&group, &image_req.name).await?;
    is!(image.bans.len(), 1, "Ban added after settings reset");
    let ban = image.bans.values().next().unwrap();
    let ban = unwrap_variant!(&ban.ban_kind, ImageBanKind::InvalidHostPath);
    is!(ban.host_path, PathBuf::from(initially_valid_path));
    // attempt to reset settings as a regular user
    let user_client = generators::client(&client).await?;
    let resp = user_client
        .system
        .reset_settings(&SystemSettingsResetParams::default())
        .await;
    fail!(resp, 401);
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn update_host_path_whitelist() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // reset settings without scanning
    client
        .system
        .reset_settings(&SystemSettingsResetParams::default().no_scan())
        .await?;
    // get user client
    let user_client = generators::client(&client).await?;
    // create group
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    let settings_update_default_params = SystemSettingsUpdateParams::default();
    // update with an invalid relative host path on the whitelist
    let update = SystemSettingsUpdate::default()
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_path("invalid/relative"));
    let resp = client
        .system
        .update_settings(&update, &settings_update_default_params)
        .await;
    fail!(resp, 400, "relative traversal");
    // make sure the settings were not updated
    let settings = client.system.get_settings().await?;
    is_not!(&settings, &update);
    // update with a host path that is absolute but has relative traversal on the whitelist
    let update = SystemSettingsUpdate::default().host_path_whitelist(
        HostPathWhitelistUpdate::default().add_path("/absolute/../but/has/.."),
    );
    let resp = client
        .system
        .update_settings(&update, &settings_update_default_params)
        .await;
    fail!(resp, 400, "relative traversal");
    // add a path to the whitelist
    let whitelisted_path = "/allowed/path";
    let update = SystemSettingsUpdate::default()
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_path(whitelisted_path));
    client
        .system
        .update_settings(&update, &settings_update_default_params)
        .await?;
    // create an image with that whitelisted host path
    let initially_valid_path = String::from(whitelisted_path) + "/here";
    let image_req = generators::gen_host_path(&group, &initially_valid_path);
    user_client.images.create(&image_req).await?;
    // change the settings to remove that host path from the whitelist
    client
        .system
        .update_settings(
            &SystemSettingsUpdate::default().host_path_whitelist(
                HostPathWhitelistUpdate::default().remove_path(whitelisted_path),
            ),
            &settings_update_default_params,
        )
        .await?;
    // make sure that image has a ban with that path now that it's no longer whitelisted
    let image = user_client.images.get(&group, &image_req.name).await?;
    is!(image.bans.len(), 1, "1 ban check after whitelist update");
    let ban = image.bans.values().next().unwrap();
    let ban = unwrap_variant!(&ban.ban_kind, ImageBanKind::InvalidHostPath);
    is!(ban.host_path, PathBuf::from(&initially_valid_path));
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn host_path_consistency_scan() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // reset settings to default
    client
        .system
        .reset_settings(&SystemSettingsResetParams::default().no_scan())
        .await?;
    // attempt to perform a successful scan
    client.system.consistency_scan().await?;
    // get user client
    let user_client = generators::client(&client).await?;
    // attempt to perform a scan as a regular user and expect a 401
    let resp = user_client.system.consistency_scan().await;
    fail!(resp, 401);
    // add a path to the whitelist
    let whitelisted_path = "/whitelisted/path";
    let update = SystemSettingsUpdate::default()
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_path(whitelisted_path));
    client
        .system
        .update_settings(&update, &SystemSettingsUpdateParams::default().no_scan())
        .await?;
    // create group
    let group = generators::groups(1, &user_client).await?.remove(0).name;
    // create an image with paths on the whitelist
    let initially_valid_paths = [
        String::from(whitelisted_path) + "/here",
        String::from(whitelisted_path) + "/and/here",
    ];
    let mut image_req = generators::gen_host_path(&group, &initially_valid_paths[0]);
    image_req.volumes.push(Volume::new(
        "volume2",
        &initially_valid_paths[1],
        VolumeTypes::HostPath,
    ));
    user_client.images.create(&image_req).await?;
    // create a pipeline with that image
    let pipeline_name = "pipeline-ban-cs";
    let pipe_req = PipelineRequest::new(
        &group,
        pipeline_name,
        serde_json::json!(vec![vec![&image_req.name]]),
    );
    client.pipelines.create(&pipe_req).await?;
    // reset settings without scanning
    client
        .system
        .reset_settings(&SystemSettingsResetParams::default().no_scan())
        .await?;
    // run a scan
    client.system.consistency_scan().await?;
    // convert path Strings to PathBuf's
    let mut initially_valid_paths_pb: HashSet<PathBuf> =
        initially_valid_paths.iter().map(PathBuf::from).collect();
    // make sure that image has two bans with those paths now that they're no longer whitelisted
    let image = user_client.images.get(&group, &image_req.name).await?;
    is!(image.bans.len(), 2, "2 image bans after consistency scan");
    let ban = image.bans.values().next().unwrap();
    let ban = unwrap_variant!(&ban.ban_kind, ImageBanKind::InvalidHostPath);
    contains!(initially_valid_paths_pb, &ban.host_path);
    let ban = image.bans.values().next().unwrap();
    let ban_kind = unwrap_variant!(&ban.ban_kind, ImageBanKind::InvalidHostPath);
    contains!(initially_valid_paths_pb, &ban_kind.host_path);
    // make sure the pipeline has a ban
    let pipeline = user_client.pipelines.get(&group, &pipe_req.name).await?;
    is!(
        pipeline.bans.len(),
        1,
        "1 pipeline ban after consistency scan"
    );
    let ban = pipeline.bans.values().next().unwrap();
    let ban_kind = unwrap_variant!(&ban.ban_kind, PipelineBanKind::BannedImage);
    is!(ban_kind.image, image.name, "Pipeline ban is from image");
    // whitelist the second path but don't run an automatic scan
    let update = SystemSettingsUpdate::default().host_path_whitelist(
        HostPathWhitelistUpdate::default().add_path(&initially_valid_paths[1]),
    );
    client
        .system
        .update_settings(&update, &SystemSettingsUpdateParams::default().no_scan())
        .await?;
    // now scan, removing the ban with the whitelisted path
    client.system.consistency_scan().await?;
    // make sure that image now has only one ban
    initially_valid_paths_pb.remove(&PathBuf::from(&initially_valid_paths[1]));
    let image = user_client.images.get(&group, &image_req.name).await?;
    is!(image.bans.len(), 1, "1 ban after whitelist");
    let ban = image.bans.values().next().unwrap();
    let ban = unwrap_variant!(&ban.ban_kind, ImageBanKind::InvalidHostPath);
    contains!(initially_valid_paths_pb, &ban.host_path);
    // allow unrestricted host paths
    let update = SystemSettingsUpdate::default().allow_unrestricted_host_paths(true);
    client
        .system
        .update_settings(&update, &SystemSettingsUpdateParams::default().no_scan())
        .await?;
    // scan, removing the final ban
    client.system.consistency_scan().await?;
    // make sure the ban is gone
    let image = user_client.images.get(&group, &image_req.name).await?;
    is!(
        image.bans.len(),
        0,
        "Empty image bans after allow unrestricted"
    );
    // make sure the pipeline's ban is gone
    let pipeline = user_client.pipelines.get(&group, &pipe_req.name).await?;
    is!(
        pipeline.bans.len(),
        0,
        "Empty pipeline bans after allow unrestricted"
    );
    // create another image when all host paths are allowed
    let unrestricted_path = "/some/random/path";
    let image_req = generators::gen_host_path(&group, unrestricted_path);
    user_client.images.create(&image_req).await?;
    // add the image to the pipeline
    let mut order: Vec<Vec<String>> = pipeline.order.clone();
    order[0].push(image_req.name.clone());
    let pipe_update = PipelineUpdate::default().order(order);
    user_client
        .pipelines
        .update(&group, &pipeline.name, &pipe_update)
        .await?;
    // restrict host paths
    let update = SystemSettingsUpdate::default().allow_unrestricted_host_paths(false);
    client
        .system
        .update_settings(&update, &SystemSettingsUpdateParams::default().no_scan())
        .await?;
    // scan, adding a ban
    client.system.consistency_scan().await?;
    // make sure both images have bans again
    // image 1
    let image = user_client.images.get(&group, &image.name).await?;
    is!(image.bans.len(), 1, "1 ban after restricted 1");
    let ban = image.bans.values().next().unwrap();
    let ban_kind = unwrap_variant!(&ban.ban_kind, ImageBanKind::InvalidHostPath);
    contains!(initially_valid_paths_pb, &ban_kind.host_path);
    // image 2
    let new_image = user_client.images.get(&group, &image_req.name).await?;
    is!(new_image.bans.len(), 1, "1 ban after restricted 2");
    let ban = new_image.bans.values().next().unwrap();
    let ban_kind = unwrap_variant!(&ban.ban_kind, ImageBanKind::InvalidHostPath);
    is!(&PathBuf::from(unrestricted_path), &ban_kind.host_path);
    // make sure the pipeline has bans from both images
    let pipeline = user_client.pipelines.get(&group, &pipe_req.name).await?;
    is!(pipeline.bans.len(), 2, "2 pipeline ban after restricted");
    let mut pipeline_banned_images = Vec::with_capacity(2);
    for ban in pipeline.bans.values() {
        pipeline_banned_images
            .push(&unwrap_variant!(&ban.ban_kind, PipelineBanKind::BannedImage).image);
    }
    contains!(pipeline_banned_images, &&new_image.name);
    contains!(pipeline_banned_images, &&image.name);
    Ok(())
}

/// Worker registration must require admin privileges.
///
/// `POST /api/system/worker/{scaler}` historically discarded the caller entirely, letting any
/// authenticated low-privilege user write attacker-chosen rows into the scaler's worker registry
/// (a scheduling-plane DoS, and on reactor deployments a path to admin-token theft). Every sibling
/// `/api/system/*` method is admin-gated. The secure behavior is that a non-admin registration is
/// rejected with `401`.
#[tokio::test]
async fn worker_registration_requires_admin() -> Result<(), Error> {
    // get an admin client just to resolve the API host and create the low-priv user
    let admin = test_utilities::admin_client().await?;
    // get a low privilege (non-admin) user client
    let attacker = generators::client(&admin).await?;
    // resolve the attacker's own username to set as the phantom worker's owner
    let username = attacker.users.info().await?.username;
    // build a phantom worker registration naming plausible cluster/node values
    let workers = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        "phantom-worker",
        username,
        "attacker-group",
        "attacker-pipeline",
        "attacker-stage",
        Resources::default(),
        Pools::Deadline,
    );
    // a non-admin registering workers must be rejected
    test_utilities::assert_status(
        attacker
            .system
            .register_workers(ImageScaler::K8s, &workers)
            .await,
        401,
        "non-admin worker registration",
    )
}

/// Deleting a batch of workers is all or nothing
///
/// `delete_workers` checks workers 25 at a time but deletes nothing until every chunk has been
/// checked, so a missing worker in a later chunk fails the request without leaving the earlier
/// chunks deleted (a partially applied delete).
#[tokio::test]
async fn delete_workers_is_all_or_nothing() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // get our username to own our workers
    let username = client.users.info().await?.username;
    // register 29 workers with unique names
    let prefix = generators::gen_string(16);
    let names: Vec<String> = (0..29).map(|i| format!("{prefix}-{i}")).collect();
    let mut registrations = WorkerRegistrationList::default();
    for name in &names {
        registrations = registrations.add(
            "cluster0",
            "node0",
            name,
            &username,
            "group",
            "pipeline",
            "stage",
            Resources::default(),
            Pools::Deadline,
        );
    }
    client
        .system
        .register_workers(ImageScaler::K8s, &registrations)
        .await?;
    // delete them all with a missing worker placed in the second chunk of 25
    let mut deletes = WorkerDeleteMap::default();
    for name in &names[..25] {
        deletes.add_mut(name);
    }
    deletes.add_mut(format!("{prefix}-ghost"));
    for name in &names[25..] {
        deletes.add_mut(name);
    }
    let result = client
        .system
        .delete_workers(ImageScaler::K8s, &deletes)
        .await;
    // count how many of our real workers still exist
    let mut remaining = 0;
    for name in &names {
        if client.system.get_worker(name).await.is_ok() {
            remaining += 1;
        }
    }
    // a failed delete must leave every worker and a successful one must leave none
    let expected = if result.is_err() { names.len() } else { 0 };
    is!(remaining, expected);
    Ok(())
}

/// A non-admin deleting a worker that doesn't exist gets a 404, not a 500
///
/// The owner lookup for a missing worker returns nil, which is reported as the worker not being
/// found rather than as a redis type error.
#[tokio::test]
async fn non_admin_deleting_missing_worker_is_not_found() -> Result<(), Error> {
    // get an admin client to create our non-admin user
    let admin = test_utilities::admin_client().await?;
    let user = generators::client(&admin).await?;
    // try to delete a worker that never existed
    let deletes = WorkerDeleteMap::default().add(generators::gen_string(24));
    test_utilities::assert_status(
        user.system.delete_workers(ImageScaler::K8s, &deletes).await,
        404,
        "non-admin delete of a missing worker",
    )
}

/// The system info an admin gets names the namespace the API serves, and a body from an API that
/// doesn't report one parses with none
#[tokio::test]
async fn info_names_the_namespace() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // the info names the namespace in the API's config
    let info = client.system.get_info(None).await?;
    is!(
        info.namespace.as_deref(),
        Some(test_utilities::CONF.thorium.namespace.as_str())
    );
    // an older API's body has no namespace field
    let old: SystemInfo = serde_json::from_str(
        r#"{"k8s_cache":false,"bare_metal_cache":false,"windows_cache":false,"kvm_cache":false,"external_cache":false}"#,
    )
    .map_err(|err| Error::new(format!("failed to parse an older system info: {err}")))?;
    is!(old.namespace, None::<String>);
    Ok(())
}

/// Reads every key under a prefix with its serialized value, so two reads can be compared
///
/// # Arguments
///
/// * `prefix` - The key prefix to read, such as `<namespace>:`
async fn redis_snapshot(prefix: &str) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    // connect to the API's Redis
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool
        .get()
        .await
        .map_err(|err| Error::new(format!("failed to get a Redis connection: {err}")))?;
    // walk every key under the prefix, escaping the glob characters a prefix could hold
    let escaped: String = prefix
        .chars()
        .flat_map(|chr| match chr {
            '*' | '?' | '[' | ']' | '\\' => vec!['\\', chr],
            _ => vec![chr],
        })
        .collect();
    let pattern = format!("{escaped}*");
    let mut keys = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next, page): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("failed to scan Redis: {err}")))?;
        keys.extend(page);
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    // serialize each key's value with DUMP
    let mut snapshot = BTreeMap::new();
    for key in keys {
        let value: Option<Vec<u8>> = redis::cmd("DUMP")
            .arg(&key)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("failed to dump {key}: {err}")))?;
        if let Some(value) = value {
            snapshot.insert(key, value);
        }
    }
    Ok(snapshot)
}

/// Restores a backup that must be refused, returning the problems the refusal lists
///
/// # Arguments
///
/// * `client` - The admin client
/// * `backup` - The backup to restore
async fn refused_restore(client: &Thorium, backup: &Backup) -> Result<Vec<Value>, Error> {
    // the restore must fail with a 400
    let Err(err) = client.system.restore(backup).await else {
        return Err(Error::new("an invalid backup was restored"));
    };
    is!(err.status(), Some(StatusCode::BAD_REQUEST));
    // its body lists every problem
    let body: Value = serde_json::from_str(&err.msg().unwrap_or_default())
        .map_err(|parse| Error::new(format!("the refusal's body isn't JSON ({parse}): {err}")))?;
    let problems = body["problems"]
        .as_array()
        .cloned()
        .ok_or_else(|| Error::new(format!("the refusal lists no problems: {body}")))?;
    Ok(problems)
}

/// Returns whether any listed problem's key and error mention a text
///
/// # Arguments
///
/// * `problems` - The problems a refusal listed
/// * `text` - The text to look for
fn names(problems: &[Value], text: &str) -> bool {
    // a problem names the text in its key or its error
    problems.iter().any(|problem| {
        problem["key"]
            .as_str()
            .is_some_and(|key| key.contains(text))
            || problem["error"]
                .as_str()
                .is_some_and(|error| error.contains(text))
    })
}

/// A restore of a backup whose records don't fit together is refused with every problem listed,
/// before any key of the namespace changes
#[serial_test::serial]
#[tokio::test]
async fn restore_refuses_an_invalid_backup_before_touching_redis() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // a group with images and pipelines, and a backup of it
    generators::gen_all(2, &client).await?;
    let backup = client.system.backup().await?;
    let prefix = format!("{}:", test_utilities::CONF.thorium.namespace);
    let before = redis_snapshot(&prefix).await?;
    // a group naming a member the backup's users don't hold
    let mut missing_member: Backup = serde_json::from_value(serde_json::to_value(&backup)?)?;
    let group = missing_member
        .groups
        .iter_mut()
        .find(|group| !group.users.direct.is_empty() || !group.owners.direct.is_empty())
        .ok_or_else(|| Error::new("the backup holds no group with members"))?;
    group.users.direct.insert("ghost-member-bv".to_owned());
    group.users.combined.insert("ghost-member-bv".to_owned());
    let problems = refused_restore(&client, &missing_member).await?;
    is!(names(&problems, "ghost-member-bv"), true);
    is!(redis_snapshot(&prefix).await?, before);
    // an image held twice
    let mut duplicate_image: Backup = serde_json::from_value(serde_json::to_value(&backup)?)?;
    let image = duplicate_image.images[0].clone();
    duplicate_image.images.push(image.clone());
    let problems = refused_restore(&client, &duplicate_image).await?;
    is!(names(&problems, &image.name), true);
    is!(redis_snapshot(&prefix).await?, before);
    // a pipeline naming an image its group doesn't hold
    let mut missing_image: Backup = serde_json::from_value(serde_json::to_value(&backup)?)?;
    missing_image.pipelines[0]
        .order
        .push(vec!["ghost-image-bv".to_owned()]);
    let problems = refused_restore(&client, &missing_image).await?;
    is!(names(&problems, "ghost-image-bv"), true);
    is!(redis_snapshot(&prefix).await?, before);
    // all three at once are each listed
    let mut all: Backup = serde_json::from_value(serde_json::to_value(&missing_member)?)?;
    all.images.push(image.clone());
    all.pipelines[0]
        .order
        .push(vec!["ghost-image-bv".to_owned()]);
    let problems = refused_restore(&client, &all).await?;
    is!(names(&problems, "ghost-member-bv"), true);
    is!(names(&problems, &image.name), true);
    is!(names(&problems, "ghost-image-bv"), true);
    is!(redis_snapshot(&prefix).await?, before);
    Ok(())
}

/// A restore of a backup whose token values collide, so that one value would log in as another
/// holder, is refused before any key of the namespace changes: two users sharing a primary token,
/// and a scoped token holding a user's primary token
#[serial_test::serial]
#[tokio::test]
async fn restore_refuses_colliding_tokens() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // a second user and a scoped token limited to a group, and a backup of them
    let (groups, _, _) = generators::gen_all(1, &client).await?;
    let user = generators::users(1, &client).await?.remove(0);
    let scoped_name = generators::gen_string(12).to_lowercase();
    client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(&scoped_name).group(&groups[0].name))
        .await?;
    let backup = client.system.backup().await?;
    let prefix = format!("{}:", test_utilities::CONF.thorium.namespace);
    let before = redis_snapshot(&prefix).await?;
    let admin_token = backup
        .users
        .iter()
        .find(|held| held.username == "thorium")
        .map(|held| held.token.clone())
        .ok_or_else(|| Error::new("the backup holds no admin"))?;
    // the second user holds the admin's primary token
    let mut shared_primary: Backup = serde_json::from_value(serde_json::to_value(&backup)?)?;
    shared_primary
        .users
        .iter_mut()
        .find(|held| held.username == user)
        .ok_or_else(|| Error::new("the backup holds no second user"))?
        .token
        .clone_from(&admin_token);
    let problems = refused_restore(&client, &shared_primary).await?;
    is!(names(&problems, "same token"), true);
    is!(redis_snapshot(&prefix).await?, before);
    // the scoped token holds the admin's primary token
    let mut scoped_primary: Backup = serde_json::from_value(serde_json::to_value(&backup)?)?;
    scoped_primary
        .scoped_tokens
        .iter_mut()
        .find(|token| token.name == scoped_name)
        .ok_or_else(|| Error::new("the backup holds no scoped token"))?
        .token
        .clone_from(&admin_token);
    let problems = refused_restore(&client, &scoped_primary).await?;
    is!(names(&problems, &scoped_name), true);
    is!(names(&problems, "primary token"), true);
    is!(redis_snapshot(&prefix).await?, before);
    // no refusal echoes the token value
    is!(
        problems
            .iter()
            .any(|problem| problem.to_string().contains(&admin_token)),
        false
    );
    Ok(())
}

/// A restore deletes only its own namespace's keys: every key of a namespace whose name extends
/// this one's is left unchanged, and no key of this namespace that the backup doesn't hold is left
#[serial_test::serial]
#[tokio::test]
async fn restore_deletes_only_its_own_namespace() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let ns = test_utilities::CONF.thorium.namespace.clone();
    // keys of neighboring namespaces, of each type, and a stray key of this one
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool
        .get()
        .await
        .map_err(|err| Error::new(format!("failed to get a Redis connection: {err}")))?;
    let neighbors = [format!("{ns}2"), format!("{ns}_x"), format!("{ns}x")];
    for neighbor in &neighbors {
        let () = redis::pipe()
            .cmd("SET")
            .arg(format!("{neighbor}:users_token_map_str"))
            .arg("kept")
            .cmd("HSET")
            .arg(format!("{neighbor}:user_data:alice"))
            .arg("username")
            .arg("alice")
            .cmd("SADD")
            .arg(format!("{neighbor}:groups"))
            .arg("kept-group")
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("failed to seed {neighbor}: {err}")))?;
    }
    let () = redis::cmd("SET")
        .arg(format!("{ns}:stray_key_bv"))
        .arg("stale")
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("failed to seed a stray key: {err}")))?;
    let neighbors_before = {
        let mut all = BTreeMap::new();
        for neighbor in &neighbors {
            all.extend(redis_snapshot(&format!("{neighbor}:")).await?);
        }
        all
    };
    is!(neighbors_before.len(), 9);
    // back up and restore this namespace
    generators::gen_all(1, &client).await?;
    let backup = client.system.backup().await?;
    client.system.restore(&backup).await?;
    // the neighbors are unchanged
    let mut neighbors_after = BTreeMap::new();
    for neighbor in &neighbors {
        neighbors_after.extend(redis_snapshot(&format!("{neighbor}:")).await?);
    }
    is!(neighbors_after, neighbors_before);
    // this namespace's stray key is gone, and the restored state is the backup's
    let stray: bool = redis::cmd("EXISTS")
        .arg(format!("{ns}:stray_key_bv"))
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("failed to check the stray key: {err}")))?;
    is!(stray, false);
    let restored = client.system.backup().await?;
    is!(
        serde_json::to_value(&restored.users)?,
        serde_json::to_value(&backup.users)?
    );
    is!(restored.groups.len(), backup.groups.len());
    is!(restored.images.len(), backup.images.len());
    is!(restored.pipelines.len(), backup.pipelines.len());
    // remove the neighbors' keys
    for key in neighbors_after.keys() {
        let () = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("failed to delete {key}: {err}")))?;
    }
    Ok(())
}

/// Sets one field of a Redis hash, returning the value it held
///
/// # Arguments
///
/// * `key` - The hash
/// * `field` - The field to set
/// * `value` - The value to set it to
async fn swap_field(key: &str, field: &str, value: &str) -> Result<String, Error> {
    // connect to the API's Redis
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool
        .get()
        .await
        .map_err(|err| Error::new(format!("failed to get a Redis connection: {err}")))?;
    // read the old value and write the new one
    let (old,): (Option<String>,) = redis::pipe()
        .cmd("HGET")
        .arg(key)
        .arg(field)
        .cmd("HSET")
        .arg(key)
        .arg(field)
        .arg(value)
        .ignore()
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("failed to set {key} {field}: {err}")))?;
    old.ok_or_else(|| Error::new(format!("{key} has no {field}")))
}

/// A backup of records that don't parse fails with 422, naming every such record's key and never
/// its stored value, and succeeds once they are repaired
#[serial_test::serial]
#[tokio::test]
async fn unparsable_records_fail_the_backup() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let ns = test_utilities::CONF.thorium.namespace.clone();
    // a group with an image and a pipeline
    let (groups, images, pipes) = generators::gen_all(1, &client).await?;
    let group = &groups[0].name;
    let group_key = format!("{ns}:groups_data:{group}");
    let image_key = format!("{ns}:image_data:{group}:{}", images[0].name);
    let pipe_key = format!("{ns}:pipeline_data:{group}:{}", pipes[0].name);
    // corrupt one field of each with values of the wrong type
    let allowed = swap_field(&group_key, "allowed", r#"{"files":"secret-allowed-bv"}"#).await?;
    let resources = swap_field(&image_key, "resources", r#"{"cpu":"secret-res-bv"}"#).await?;
    let order = swap_field(&pipe_key, "order", r#""secret-order-bv""#).await?;
    // the backup fails with 422 naming the three keys
    let Err(err) = client.system.backup().await else {
        return Err(Error::new("a backup of unparsable records succeeded"));
    };
    is!(err.status(), Some(StatusCode::UNPROCESSABLE_ENTITY));
    let text = err.msg().unwrap_or_default();
    let body: Value = serde_json::from_str(&text)
        .map_err(|parse| Error::new(format!("the error body isn't JSON ({parse}): {text}")))?;
    let keys: HashSet<&str> = body["problems"]
        .as_array()
        .map_or_else(HashSet::new, |problems| {
            problems.iter().filter_map(|p| p["key"].as_str()).collect()
        });
    is!(keys.contains(group_key.as_str()), true);
    is!(keys.contains(image_key.as_str()), true);
    is!(keys.contains(pipe_key.as_str()), true);
    // no stored value is copied into the body
    for secret in ["secret-allowed-bv", "secret-res-bv", "secret-order-bv"] {
        is!(text.contains(secret), false);
    }
    // once repaired, the backup holds all three records again
    swap_field(&group_key, "allowed", &allowed).await?;
    swap_field(&image_key, "resources", &resources).await?;
    swap_field(&pipe_key, "order", &order).await?;
    let backup = client.system.backup().await?;
    is!(backup.groups.iter().any(|g| &g.name == group), true);
    is!(backup.images.iter().any(|i| i.name == images[0].name), true);
    is!(
        backup.pipelines.iter().any(|p| p.name == pipes[0].name),
        true
    );
    Ok(())
}

/// A backup whose records don't fit together, as a group member that isn't a user, fails with 422
/// naming the problem rather than returning a backup the restore would refuse, and succeeds once
/// the records are repaired
#[serial_test::serial]
#[tokio::test]
async fn a_backup_the_restore_would_refuse_fails() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let ns = test_utilities::CONF.thorium.namespace.clone();
    // a group whose members include a user that doesn't exist
    let (groups, _, _) = generators::gen_all(1, &client).await?;
    let group = &groups[0].name;
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool
        .get()
        .await
        .map_err(|err| Error::new(format!("failed to get a Redis connection: {err}")))?;
    let sets = [
        format!("{ns}:groups_direct:{group}:users"),
        format!("{ns}:groups_combined:{group}:users"),
    ];
    for set in &sets {
        let () = redis::cmd("SADD")
            .arg(set)
            .arg("ghost-member-bv")
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("failed to add a ghost member: {err}")))?;
    }
    // the backup fails with 422 naming the member
    let failed = client.system.backup().await;
    for set in &sets {
        let () = redis::cmd("SREM")
            .arg(set)
            .arg("ghost-member-bv")
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(format!("failed to remove the ghost member: {err}")))?;
    }
    let Err(err) = failed else {
        return Err(Error::new("a backup the restore would refuse succeeded"));
    };
    is!(err.status(), Some(StatusCode::UNPROCESSABLE_ENTITY));
    let text = err.msg().unwrap_or_default();
    let body: Value = serde_json::from_str(&text)
        .map_err(|parse| Error::new(format!("the error body isn't JSON ({parse}): {text}")))?;
    let problems = body["problems"].as_array().cloned().unwrap_or_default();
    is!(names(&problems, "ghost-member-bv"), true);
    // once repaired, the backup succeeds and restores
    let backup = client.system.backup().await?;
    client.system.restore(&backup).await?;
    Ok(())
}

/// Builds the URL of the test Redis, with its credentials when they are configured
fn redis_url() -> String {
    // use the same credentials the test API connects with
    let redis = &test_utilities::CONF.redis;
    match (&redis.username, &redis.password) {
        (Some(user), Some(password)) => {
            format!("redis://{user}:{password}@{}:{}/", redis.host, redis.port)
        }
        (None, Some(password)) => {
            format!("redis://default:{password}@{}:{}/", redis.host, redis.port)
        }
        _ => format!("redis://{}:{}/", redis.host, redis.port),
    }
}

/// A user deleted after a backup lists the users but before it reads them doesn't fail the
/// backup: the backup reads the records again and returns them without that user
#[serial_test::serial]
#[tokio::test]
async fn a_user_deleted_while_a_backup_reads_is_left_out() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // the keys of the user set and of the deleted user's data
    let ns = test_utilities::CONF.thorium.namespace.clone();
    let ghost = "deleted-while-backing-up";
    let users_key = format!("{ns}:users");
    let data_key = format!("{ns}:user_data:{ghost}");
    // watch every command Redis runs, so the user can be deleted right after the backup reads it
    let redis = redis::Client::open(redis_url())
        .map_err(|err| Error::new(format!("failed to open a Redis client: {err}")))?;
    let mut commands = redis
        .get_async_monitor()
        .await
        .map_err(|err| Error::new(format!("failed to open a Redis monitor: {err}")))?
        .into_on_message::<String>();
    // a user listed in the user set whose data is already gone, as a deletion leaves it mid-read
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool
        .get()
        .await
        .map_err(|err| Error::new(format!("failed to get a Redis connection: {err}")))?;
    let () = redis::cmd("SADD")
        .arg(&users_key)
        .arg(ghost)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("failed to list the deleted user: {err}")))?;
    // finish the deletion once the backup has read the user's data, and back up meanwhile
    let watched = format!("\"{data_key}\"");
    let finish_deletion = async {
        // wait for the backup's read of the user's data
        while let Some(command) = futures::StreamExt::next(&mut commands).await {
            if command.to_lowercase().contains("\"hgetall\"") && command.contains(&watched) {
                break;
            }
        }
        // then remove the user from the user set, as the deletion would
        redis::cmd("SREM")
            .arg(&users_key)
            .arg(ghost)
            .query_async::<()>(&mut *conn)
            .await
    };
    let finish_deletion = tokio::time::timeout(std::time::Duration::from_secs(60), finish_deletion);
    let (backup, finished) = tokio::join!(client.system.backup(), finish_deletion);
    // make sure the user is gone however the backup went
    let () = redis::cmd("SREM")
        .arg(&users_key)
        .arg(ghost)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("failed to remove the deleted user: {err}")))?;
    // the deletion finished while the backup ran
    finished
        .map_err(|_| Error::new("the backup never read the deleted user"))?
        .map_err(|err| Error::new(format!("failed to delete the user: {err}")))?;
    // the backup succeeds without the deleted user
    let backup = backup?;
    let listed = backup.users.iter().any(|user| user.username == ghost);
    is!(listed, false);
    is!(backup.users.is_empty(), false);
    Ok(())
}

/// A restore brings back settings changed from their defaults
#[serial_test::serial]
#[tokio::test]
async fn restore_brings_back_changed_settings() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // change the reserved cpu and the host path whitelist from their defaults
    let update = SystemSettingsUpdate::default()
        .reserved_cpu("1700m")
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_path("/mnt/restored-bv"));
    client
        .system
        .update_settings(&update, &SystemSettingsUpdateParams::default().no_scan())
        .await?;
    let changed = client.system.get_settings().await?;
    is_not!(&changed, &SystemSettings::default());
    // back up and restore
    let backup = client.system.backup().await?;
    client.system.restore(&backup).await?;
    // the settings are the changed ones
    is!(client.system.get_settings().await?, changed);
    // put the defaults back for the other tests
    client
        .system
        .reset_settings(&SystemSettingsResetParams::default().no_scan())
        .await?;
    Ok(())
}

/// The whole Redis state a test changes for the restore of every Redis record type
struct RedisState {
    /// The group whose repos are disallowed
    group: String,
    /// The banned image, which uses the network policy
    image: String,
    /// The pipeline banned on its own
    pipeline: String,
    /// The network policy the image uses
    policy: String,
    /// A network policy an image used and then stopped using, which the group's set still holds
    unused_policy: String,
    /// The scoped token's value
    scoped: String,
    /// The user with a verification time
    user: String,
}

/// Seeds a group without repos, a banned image using a network policy, a network policy no image
/// uses any more, a banned pipeline, a scoped token, and a user with a verification time
///
/// # Arguments
///
/// * `client` - The admin client
async fn seed_redis_state(client: &Thorium) -> Result<RedisState, Error> {
    // a group with images and pipelines, and two network policies in it
    let (groups, images, pipes) = generators::gen_all(1, client).await?;
    let group = groups[0].name.clone();
    let mut policies =
        generators::network_policies(std::slice::from_ref(&group), 2, client).await?;
    let unused_policy = policies.remove(1).name;
    let policy = policies.remove(0).name;
    // the group allows no repos
    let update = GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_repos(),
        ..GroupUpdate::default()
    };
    client.groups.update(&group, &update).await?;
    // an image uses the policy and is banned
    let image = images[0].name.clone();
    let update = ImageUpdate::default()
        .bans(ImageBanUpdate::default().add_ban(ImageBan::new(ImageBanKind::generic("bv ban"))))
        .network_policies(ImageNetworkPolicyUpdate::default().add_policies([policy.clone()]));
    client.images.update(&group, &image, &update).await?;
    // the image uses the other policy, then stops, which leaves it in the group's set alone
    let update = ImageUpdate::default().network_policies(
        ImageNetworkPolicyUpdate::default().add_policies([unused_policy.clone()]),
    );
    client.images.update(&group, &image, &update).await?;
    let update = ImageUpdate::default().network_policies(
        ImageNetworkPolicyUpdate::default().remove_policies([unused_policy.clone()]),
    );
    client.images.update(&group, &image, &update).await?;
    // a pipeline is banned on its own
    let pipeline = pipes[0].name.clone();
    let update = PipelineUpdate::default().bans(
        PipelineBanUpdate::default().add_ban(PipelineBan::new(PipelineBanKind::generic("bv ban"))),
    );
    client.pipelines.update(&group, &pipeline, &update).await?;
    // the admin holds a scoped token limited to the group
    let scoped = client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(generators::gen_string(12)).group(&group))
        .await?
        .token;
    // a user was sent a verification email
    let user = generators::users(1, client).await?.remove(0);
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool
        .get()
        .await
        .map_err(|err| Error::new(format!("failed to get a Redis connection: {err}")))?;
    let () = redis::cmd("HSET")
        .arg(format!(
            "{}:user_data:{user}",
            test_utilities::CONF.thorium.namespace
        ))
        .arg("verification_sent")
        .arg(r#""2026-10-01T12:00:00Z""#)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(format!("failed to set verification_sent: {err}")))?;
    Ok(RedisState {
        group,
        image,
        pipeline,
        policy,
        unused_policy,
        scoped,
        user,
    })
}

/// Returns when a user in a backup was sent a verification email
///
/// # Arguments
///
/// * `backup` - The backup
/// * `username` - The user
fn verification_sent(backup: &Backup, username: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // the user's field, if the backup holds the user
    backup
        .users
        .iter()
        .find(|user| user.username == username)
        .and_then(|user| user.verification_sent)
}

/// Group `allowed`, image and pipeline bans, scoped tokens, network-policy sets (a policy no
/// image uses any more included), and `verification_sent` all survive a backup and restore, and
/// keep working
#[serial_test::serial]
#[tokio::test]
async fn restore_brings_back_the_whole_redis_state() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let state = seed_redis_state(&client).await?;
    // back up and restore
    let backup = client.system.backup().await?;
    let sent = verification_sent(&backup, &state.user);
    is!(sent.is_some(), true);
    is!(
        backup.scoped_tokens.iter().any(|t| t.token == state.scoped),
        true
    );
    is!(
        backup
            .network_policy_sets
            .iter()
            .any(|set| set.group == state.group
                && set.policies.contains(&state.policy)
                && set.policies.contains(&state.unused_policy)),
        true
    );
    client.system.restore(&backup).await?;
    // the group still refuses repos
    let repo = RepoRequest::new("github.com/bv/restored", vec![state.group.clone()], None);
    let refused = client
        .repos
        .create(&repo)
        .await
        .err()
        .and_then(|err| err.status());
    is!(refused, Some(StatusCode::UNAUTHORIZED));
    // the bans are still there, and a banned pipeline still can't react
    let image = client.images.get(&state.group, &state.image).await?;
    is!(image.bans.is_empty(), false);
    let pipeline = client.pipelines.get(&state.group, &state.pipeline).await?;
    is!(pipeline.bans.is_empty(), false);
    let reaction = generators::gen_reaction(&state.group, &pipeline, None);
    let reacted = client.reactions.create(&reaction).await;
    is!(reacted.is_err(), true);
    // the scoped token still authenticates
    let scoped = Thorium::build(client.host.clone())
        .token(&state.scoped)
        .build()
        .await?;
    scoped.users.info().await?;
    // renaming the policy renames it in the image that uses it
    let renamed = generators::gen_string(20).to_lowercase();
    client
        .network_policies
        .update(
            &state.policy,
            None,
            &NetworkPolicyUpdate::default().new_name(&renamed),
        )
        .await?;
    let image = client.images.get(&state.group, &state.image).await?;
    is!(image.network_policies.contains(&renamed), true);
    is!(image.network_policies.contains(&state.policy), false);
    // the verification time is unchanged
    let restored = client.system.backup().await?;
    is!(verification_sent(&restored, &state.user), sent);
    // the group still holds the network policy no image uses
    is!(
        restored
            .network_policy_sets
            .iter()
            .any(|set| set.group == state.group && set.policies.contains(&state.unused_policy)),
        true
    );
    Ok(())
}

/// A backup taken before scoped tokens and network-policy sets were backed up, which has
/// neither field, restores with no scoped tokens, and with each group's network-policy set
/// holding only the policies its restored images use, as rebuilt from the images
#[serial_test::serial]
#[tokio::test]
async fn a_backup_without_the_newer_fields_restores() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let state = seed_redis_state(&client).await?;
    // a backup as an older API writes it, without the two newer fields
    let mut older = serde_json::to_value(client.system.backup().await?)?;
    let fields = older
        .as_object_mut()
        .ok_or_else(|| Error::new("the backup isn't an object"))?;
    is!(fields.remove("scoped_tokens").is_some(), true);
    is!(fields.remove("network_policy_sets").is_some(), true);
    // restore it as an older thoradm would send it
    let token = client.users.get("thorium").await?.token;
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    let resp = reqwest::Client::new()
        .post(format!("{}/api/system/restore", client.host))
        .header("authorization", format!("token {encoded}"))
        .json(&older)
        .send()
        .await?;
    is!(resp.status(), StatusCode::NO_CONTENT);
    // no scoped token is left, and each network-policy set is rebuilt from the images
    let restored = client.system.backup().await?;
    is!(restored.scoped_tokens.len(), 0);
    for set in &restored.network_policy_sets {
        let used: HashSet<&String> = restored
            .images
            .iter()
            .filter(|image| image.group == set.group)
            .flat_map(|image| image.network_policies.iter())
            .collect();
        is!(
            set.policies.iter().all(|policy| used.contains(policy)),
            true
        );
    }
    is!(
        restored
            .network_policy_sets
            .iter()
            .any(|set| set.group == state.group && set.policies.contains(&state.policy)),
        true
    );
    is!(
        restored.users.len(),
        older["users"].as_array().map_or(0, Vec::len)
    );
    Ok(())
}

/// Send a GET to the API with this client's credentials and return its status and JSON body
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path and query to request, starting with `/api/`
async fn get_json(client: &Thorium, path: &str) -> Result<(u16, serde_json::Value), Error> {
    // get the token for this client
    let token = client.users.info().await?.token;
    // build the auth header the client would send
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    // send our raw request so every query parameter reaches the API unchanged
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", client.host))
        .header("authorization", format!("token {encoded}"))
        .send()
        .await?;
    // get the status and body of the response
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body));
    Ok((status, json))
}

/// Getting nodes by name works when the cluster and node names span 100 or more pairs, which
/// queries the nodes in chunks
#[serial_test::serial]
#[tokio::test]
async fn get_many_nodes_in_chunks() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register one node in a cluster of its own
    let cluster = format!("chunks-{}", uuid::Uuid::new_v4().simple());
    generators::node(&cluster, "n0", Resources::default(), &client).await?;
    // ask for that node among 100 and then 150 node names, on both node listing routes
    for count in [100, 150] {
        let nodes = (0..count)
            .map(|index| format!("&nodes[]=n{index}"))
            .collect::<Vec<String>>()
            .concat();
        for (route, field) in [("nodes/", "node"), ("nodes/details/", "name")] {
            let path = format!("/api/system/{route}?clusters[]={cluster}{nodes}");
            let (status, body) = get_json(&client, &path).await?;
            is!(
                status,
                200,
                format!("{route} with {count} nodes returned {body}")
            );
            // only the node we registered exists
            let names = body["data"].as_array().map_or_else(Vec::new, |rows| {
                rows.iter().map(|row| row[field].clone()).collect()
            });
            is!(
                names,
                vec![serde_json::json!("n0")],
                format!("{route} with {count} nodes")
            );
        }
    }
    Ok(())
}
