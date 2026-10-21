//! The graphics bucket and the results bucket's entity blobs survive backup, wipe, and restore,
//! and the graphics component backs up the graphics the backup's own rows name
//!
//! Every test holds the binary's restore guard, so no other test's backup sees the graphics a
//! test removes or adds.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::cycle::{backup, describe, failure_records, restore, restore_with, wipe};
use common::harness::{Harness, bucket_for, list_keys};
use common::names::Names;
use common::seed;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Output;
use thoradm::args::BackupComponents;
use thoradm::backup::manifest::{InProgressMarker, MANIFEST_FILE, Manifest, ObjectIndexLine};
use thoradm::backup::sink::TokioFs;
use thorium::models::EntityKinds;
use uuid::Uuid;

/// How many characters of an object's key name the directory a backup spreads its objects over
const OBJECT_PREFIX_CHARS: usize = 6;

/// Reads every object of a bucket role whose key passes a filter, by key
///
/// # Arguments
///
/// * `harness` - The harness naming the buckets
/// * `role` - The bucket role
/// * `keep` - Which keys to read
async fn read_objects(
    harness: &Harness,
    role: &str,
    keep: impl Fn(&str) -> bool,
) -> BTreeMap<String, Vec<u8>> {
    // list the bucket and read each kept object's bytes
    let s3 = harness.s3();
    let bucket = bucket_for(&harness.conf, role);
    let mut objects = BTreeMap::new();
    for key in list_keys(&s3, &bucket).await.unwrap() {
        if !keep(&key) {
            continue;
        }
        let object = s3
            .get_object()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .unwrap_or_else(|err| panic!("failed to read {bucket}/{key}: {err}"));
        let bytes = object.body.collect().await.unwrap().into_bytes().to_vec();
        objects.insert(key, bytes);
    }
    objects
}

/// Returns where a backup keeps an object of a component: under the component's `objects/`
/// directory, the first characters of the key, and then the key
///
/// # Arguments
///
/// * `dir` - The backup
/// * `component_dir` - The component's directory in the backup
/// * `key` - The object's key
fn kept_at(dir: &Path, component_dir: &str, key: &str) -> PathBuf {
    // the key's first characters spread the objects over directories
    let prefix: String = key.chars().take(OBJECT_PREFIX_CHARS).collect();
    dir.join(component_dir)
        .join("objects")
        .join(prefix)
        .join(key)
}

/// Returns whether a results-bucket key is an entity blob (`{id}/__thorium_entities/{kind}.json`)
///
/// # Arguments
///
/// * `key` - The object's key
fn is_entity_blob(key: &str) -> bool {
    // the blobs sit under their result's id in a fixed directory
    key.split('/').nth(1) == Some("__thorium_entities")
}

/// A user icon, an entity image, and a result that found a device: the backup holds the graphics
/// objects and the result's `Device.json` entity blob, and after a wipe and a restore every one
/// of them reads back byte for byte
#[tokio::test]
async fn graphics_and_entity_blobs_come_back_byte_for_byte() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    // start from what a fresh install holds
    wipe(harness).await;
    let client = harness.client().await;
    client
        .system
        .init()
        .await
        .expect("failed to initialize the system");
    // a profile icon, a device entity with an image, and a result that found one device
    let names = Names::new();
    let group = names.group("vulcan");
    seed::group(&client, &group).await;
    seed::icon(&harness.api_url, &harness.token).await;
    let image = b"\x89PNG\r\n\x1a\nforge device image";
    seed::entity_image(
        &harness.api_url,
        &harness.token,
        &group,
        &names.user("adrastea"),
        image,
    )
    .await;
    let sample = seed::sample(&client, &group, "forge.bin", b"forge sample").await;
    let blob = br#"[{"name":"forge device"}]"#;
    let result =
        seed::result_with_entities(&client, &sample, &[(EntityKinds::Device, 1, blob)]).await;
    // what the buckets hold before the backup
    let graphics = read_objects(harness, "graphics", |_| true).await;
    let blobs = read_objects(harness, "results", is_entity_blob).await;
    let device = format!("{result}/__thorium_entities/Device.json");
    assert!(
        graphics.len() >= 2,
        "the icon and the entity image are in the graphics bucket: {:?}",
        graphics.keys()
    );
    assert_eq!(
        blobs.keys().collect::<Vec<_>>(),
        vec![&device],
        "the result's entity blob is in the results bucket"
    );
    // the backup holds every graphics object and the entity blob, byte for byte
    let dir = harness.scratch("graphics").join("backup");
    let _ = std::fs::remove_dir_all(&dir);
    let taken = backup(harness, &dir);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    let mut problems = Vec::new();
    let kept = graphics
        .iter()
        .map(|(key, bytes)| (kept_at(&dir, "graphics", key), key, bytes))
        .chain(
            blobs
                .iter()
                .map(|(key, bytes)| (kept_at(&dir, "results", key), key, bytes)),
        );
    for (path, key, bytes) in kept {
        match std::fs::read(&path) {
            Ok(found) if found == *bytes => (),
            Ok(_) => problems.push(format!("the backup's copy of {key} differs")),
            Err(err) => problems.push(format!(
                "the backup lacks {key} at {}: {err}",
                path.display()
            )),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    // after a wipe and a restore every object reads back byte for byte
    wipe(harness).await;
    let restored = restore(harness, &dir);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    assert_eq!(
        read_objects(harness, "graphics", |_| true).await,
        graphics,
        "the graphics bucket came back unchanged"
    );
    assert_eq!(
        read_objects(harness, "results", is_entity_blob).await,
        blobs,
        "the entity blobs came back unchanged"
    );
}

/// The graphics a seeded cluster holds, each named by a row
struct Seeded {
    /// The admin's icon, named by the admin's `image` in Redis
    icon: String,
    /// The device's image, named by the entity row's `image` column
    image: String,
}

/// Empties the cluster, seeds an icon and an entity image through the API, and returns their keys
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
async fn seed_graphics(harness: &Harness) -> Seeded {
    // start from what a fresh install holds
    wipe(harness).await;
    let client = harness.client().await;
    client
        .system
        .init()
        .await
        .expect("failed to initialize the system");
    // a group, the admin's icon, and a device with an image
    let names = Names::new();
    let group = names.group("ceres");
    seed::group(&client, &group).await;
    seed::icon(&harness.api_url, &harness.token).await;
    let id = seed::entity_image(
        &harness.api_url,
        &harness.token,
        &group,
        &names.user("pallas"),
        b"\x89PNG\r\n\x1a\nceres device image",
    )
    .await;
    // the keys the API stored them under: the entity's id names its image
    let bucket = bucket_for(&harness.conf, "graphics");
    let keys = list_keys(&harness.s3(), &bucket).await.unwrap();
    let image = keys
        .iter()
        .find(|key| key.starts_with(&id.to_string()))
        .unwrap_or_else(|| panic!("no image of entity {id} in {keys:?}"))
        .clone();
    let icon = keys
        .iter()
        .find(|key| key.starts_with("users/") && key.contains("/icon."))
        .unwrap_or_else(|| panic!("no icon in {keys:?}"))
        .clone();
    Seeded { icon, image }
}

/// Runs `thoradm backup new` for some components into a directory
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `components` - The comma-separated component list, or `None` for the default (all)
/// * `dir` - The backup directory
fn backup_of(harness: &Harness, components: Option<&str>, dir: &Path) -> Output {
    // a small worker count and chunk multiplier keep the run quick
    let mut command = harness.thoradm();
    command.args(["--workers", "4", "backup", "new"]);
    if let Some(components) = components {
        command.arg(components);
    }
    command
        .args(["-m", "4", "-o"])
        .arg(dir)
        .output()
        .expect("failed to run thoradm")
}

/// Returns the keys of the graphics a backup's object index lists
///
/// # Arguments
///
/// * `dir` - The backup
fn indexed(dir: &Path) -> BTreeSet<String> {
    // each line of the index names one object
    let index = std::fs::read_to_string(dir.join("graphics/objects.index.jsonl"))
        .unwrap_or_else(|err| panic!("the backup has no graphics index: {err}"));
    index
        .lines()
        .map(|line| {
            serde_json::from_str::<ObjectIndexLine>(line)
                .expect("an index line parses")
                .key
        })
        .collect()
}

/// Puts a small object into the graphics bucket
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `key` - The object's key
async fn put_graphic(harness: &Harness, key: &str) {
    // a PNG-typed object, as the API stores graphics
    harness
        .s3()
        .put_object()
        .bucket(bucket_for(&harness.conf, "graphics"))
        .key(key)
        .content_type("image/png")
        .body(ByteStream::from_static(b"\x89PNG\r\n\x1a\na graphic"))
        .send()
        .await
        .unwrap_or_else(|err| panic!("failed to put {key}: {err}"));
}

/// Removes an object from the graphics bucket
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `key` - The object's key
async fn delete_graphic(harness: &Harness, key: &str) {
    // a delete of a key that is there
    harness
        .s3()
        .delete_object()
        .bucket(bucket_for(&harness.conf, "graphics"))
        .key(key)
        .send()
        .await
        .unwrap_or_else(|err| panic!("failed to delete {key}: {err}"));
}

/// An entity image and a user icon uploaded through the API are in the backup and its index,
/// which a backup of the graphics, the entities, and Redis names from the rows alone
#[tokio::test]
async fn graphics_keys_come_from_entity_rows_and_user_records() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // back up the three components graphics derives its keys from
    let dir = harness.scratch("graphics-derived").join("backup");
    let taken = backup_of(harness, Some("graphics,entities,redis"), &dir);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // the index names both graphics, and the backup holds their bytes
    let keys = indexed(&dir);
    assert!(keys.contains(&seeded.image), "{keys:?}");
    assert!(keys.contains(&seeded.icon), "{keys:?}");
    for key in [&seeded.image, &seeded.icon] {
        assert!(kept_at(&dir, "graphics", key).is_file(), "{key}");
    }
}

/// An object no row names is absent from the backup
#[tokio::test]
async fn an_orphan_graphic_is_not_backed_up() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // an object in the graphics bucket that no entity row or user record names
    let orphan = format!("{}/orphan.png", Uuid::new_v4());
    put_graphic(harness, &orphan).await;
    // a backup of everything keeps the graphics the rows name and leaves the orphan
    let dir = harness.scratch("graphics-orphan").join("backup");
    let taken = backup(harness, &dir);
    delete_graphic(harness, &orphan).await;
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    let keys = indexed(&dir);
    assert!(
        keys.contains(&seeded.image) && keys.contains(&seeded.icon),
        "{keys:?}"
    );
    assert!(!keys.contains(&orphan), "the orphan {orphan} is indexed");
    assert!(!kept_at(&dir, "graphics", &orphan).exists());
}

/// A `--wipe` restore of a backup of the graphics succeeds and puts back every graphic the
/// backup's rows name, and an object in the bucket that no restored row names is left alone
#[tokio::test]
async fn a_wipe_restore_of_graphics_puts_them_back_and_leaves_orphans() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // a backup of everything, taken while no orphan exists
    let dir = harness.scratch("graphics-wipe").join("backup");
    let taken = backup(harness, &dir);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // after it, a named graphic is deleted and an object no row names is added
    delete_graphic(harness, &seeded.image).await;
    let orphan = format!("{}/orphan.png", Uuid::new_v4());
    put_graphic(harness, &orphan).await;
    // the wipe restore finishes cleanly even though the bucket holds an object it never lists
    let namespace = harness.conf.thorium.namespace.clone();
    let restored = restore_with(harness, &dir, &["--wipe", &namespace]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the named graphics are back and the orphan survives
    let bucket = bucket_for(&harness.conf, "graphics");
    let keys: BTreeSet<String> = list_keys(&harness.s3(), &bucket)
        .await
        .unwrap()
        .into_iter()
        .collect();
    delete_graphic(harness, &orphan).await;
    assert!(keys.contains(&seeded.image), "{keys:?}");
    assert!(keys.contains(&seeded.icon), "{keys:?}");
    assert!(
        keys.contains(&orphan),
        "the wipe removed {orphan}: {keys:?}"
    );
}

/// A graphic a row names that the bucket lacks is a recorded failed object, not skipped
#[tokio::test]
async fn a_referenced_graphic_missing_in_s3_is_a_failed_object() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // the entity's image is gone from the bucket while its row still names it
    delete_graphic(harness, &seeded.image).await;
    let dir = harness.scratch("graphics-missing").join("backup");
    let taken = backup(harness, &dir);
    put_graphic(harness, &seeded.image).await;
    // the run finishes with a failure naming the graphics component and the key
    assert_eq!(taken.status.code(), Some(2), "{}", describe(&taken));
    let records = failure_records(&dir);
    assert!(
        records.iter().any(|record| record.component == "graphics"
            && record.key.as_deref() == Some(seeded.image.as_str())),
        "no failure names {}: {records:?}",
        seeded.image
    );
    // the icon that is there is still backed up
    assert!(indexed(&dir).contains(&seeded.icon));
}

/// A backup of graphics without the entities or Redis components is refused as a usage error
/// naming what is missing, and writes nothing
#[tokio::test]
async fn graphics_without_entities_or_redis_is_refused() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    // each selection with the message naming exactly the components it lacks
    for (components, message) in [
        ("graphics", "graphics needs entities and redis"),
        ("graphics,entities", "graphics needs redis"),
        ("graphics,redis", "graphics needs entities"),
    ] {
        let dir = harness.scratch("graphics-refused").join("backup");
        let refused = backup_of(harness, Some(components), &dir);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(refused.status.code(), Some(1), "{components}: {stderr}");
        assert!(stderr.contains(message), "{components}: {stderr}");
        // nothing was written, not even the directory's marker
        let written = std::fs::read_dir(&dir).map_or(0, Iterator::count);
        assert_eq!(written, 0, "{components} wrote into {}", dir.display());
    }
}

/// A backup of graphics with the entities and Redis components is accepted and backs up the
/// graphics the rows name
#[tokio::test]
async fn graphics_with_entities_and_redis_is_accepted() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // the three components in another order than the run's
    let dir = harness.scratch("graphics-accepted").join("backup");
    let taken = backup_of(harness, Some("redis,graphics,entities"), &dir);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    let keys = indexed(&dir);
    assert!(
        keys.contains(&seeded.image) && keys.contains(&seeded.icon),
        "{keys:?}"
    );
}

/// A backup of every component, with no component named, still includes the graphics
#[tokio::test]
async fn a_backup_of_all_components_is_unaffected() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // a plain backup and an explicit `all` both include the graphics
    for (name, components) in [("plain", None), ("all", Some("all"))] {
        let dir = harness.scratch(&format!("graphics-{name}")).join("backup");
        let taken = backup_of(harness, components, &dir);
        assert_eq!(taken.status.code(), Some(0), "{name}: {}", describe(&taken));
        let keys = indexed(&dir);
        assert!(
            keys.contains(&seeded.image) && keys.contains(&seeded.icon),
            "{name}: {keys:?}"
        );
    }
}

/// Resuming a backup whose recorded selection holds graphics with their prerequisites works,
/// and resuming one whose recorded selection lacks them is refused with the same message
#[tokio::test]
async fn resume_keeps_the_graphics_selection_rule() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // an interrupted backup that selected the graphics with entities and redis; the resuming
    // command names only entities, and the run keeps the interrupted run's selection
    let dir = harness.scratch("graphics-resume").join("backup");
    std::fs::create_dir_all(&dir).unwrap();
    let components = vec![
        BackupComponents::Entities,
        BackupComponents::Redis,
        BackupComponents::Graphics,
    ];
    InProgressMarker::new(chrono::Utc::now(), components)
        .write(&dir, &TokioFs)
        .await
        .unwrap();
    let resumed = backup_of(harness, Some("entities"), &dir);
    assert_eq!(resumed.status.code(), Some(0), "{}", describe(&resumed));
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE)).unwrap()).unwrap();
    assert!(manifest.components.contains(&BackupComponents::Graphics));
    let keys = indexed(&dir);
    assert!(
        keys.contains(&seeded.image) && keys.contains(&seeded.icon),
        "{keys:?}"
    );
    // an interrupted backup whose selection holds graphics without redis is refused
    let lacking = harness.scratch("graphics-resume-lacking").join("backup");
    std::fs::create_dir_all(&lacking).unwrap();
    let components = vec![BackupComponents::Entities, BackupComponents::Graphics];
    InProgressMarker::new(chrono::Utc::now(), components)
        .write(&lacking, &TokioFs)
        .await
        .unwrap();
    let refused = backup_of(harness, Some("entities"), &lacking);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("graphics needs redis"), "{stderr}");
    assert!(!lacking.join(MANIFEST_FILE).exists());
}

/// Every key in the graphics bucket after a backup is in the backup's index, unless the test
/// declares it an orphan: a key no entity row and no user names
#[tokio::test]
async fn every_bucket_key_is_in_the_index_or_declared_orphan() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let seeded = seed_graphics(harness).await;
    // one declared orphan, which the backup is allowed to leave out
    let orphan = format!("{}/orphan.png", Uuid::new_v4());
    put_graphic(harness, &orphan).await;
    let dir = harness.scratch("graphics-invariant").join("backup");
    let taken = backup(harness, &dir);
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // list the bucket in test code, after the backup
    let bucket = bucket_for(&harness.conf, "graphics");
    let keys = list_keys(&harness.s3(), &bucket).await.unwrap();
    delete_graphic(harness, &orphan).await;
    // the keys the rows name, read straight from the entities table and the users' icons
    let session = harness.scylla().await;
    let query = format!(
        "SELECT image FROM \"{}\".entities",
        harness.conf.thorium.namespace
    );
    let rows = session
        .query_unpaged(query, &[])
        .await
        .expect("failed to read the entities")
        .into_rows_result()
        .expect("the entities come back as rows");
    let mut referenced: BTreeSet<String> = rows
        .rows::<(Option<String>,)>()
        .expect("the image column decodes")
        .filter_map(|row| row.expect("a row decodes").0)
        .collect();
    let client = harness.client().await;
    for user in client.users.list_details().await.unwrap() {
        if user.has_image {
            referenced.extend(
                keys.iter()
                    .filter(|key| key.starts_with(&format!("users/{}/icon.", user.username)))
                    .cloned(),
            );
        }
    }
    assert!(referenced.contains(&seeded.image) && referenced.contains(&seeded.icon));
    // every key in the bucket is in the index, or is the declared orphan
    let index = indexed(&dir);
    let missing: Vec<&String> = keys
        .iter()
        .filter(|key| **key != orphan && !index.contains(*key))
        .collect();
    assert!(missing.is_empty(), "keys the backup lacks: {missing:?}");
    // and every key a row names is in the index
    let unindexed: Vec<&String> = referenced
        .iter()
        .filter(|key| !index.contains(*key))
        .collect();
    assert!(
        unindexed.is_empty(),
        "referenced keys the backup lacks: {unindexed:?}"
    );
}
