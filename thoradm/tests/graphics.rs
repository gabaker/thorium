//! The graphics bucket and the results bucket's entity blobs survive backup, wipe, and restore
//!
//! The test restores, so it holds the binary's restore guard.

mod common;

use common::cycle::{backup, describe, restore, wipe};
use common::harness::{Harness, bucket_for, list_keys};
use common::names::Names;
use common::seed;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thorium::models::EntityKinds;

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
