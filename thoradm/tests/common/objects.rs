//! Object layouts: which classes each backed-up bucket layout needs, the rows and objects that
//! hold them, and what the buckets actually hold
//!
//! Layouts come from the registry's backed-up S3 entries, one per prefix of a bucket backed up by
//! prefix. Every object outside the graphics bucket is named by a row (an `s3_ids` row for files
//! and repos, a comment for attachments, a result for result files and entity blobs), because a
//! backup only copies those objects some row names; the graphics bucket is copied by listing it,
//! so its objects need no row.

use super::classes::{
    CoverageMatrix, Location, MAX_S3_KEY, Need, PART_SIZE, ValueClass, is_odd_key, odd_key,
};
use super::columns::{Body, LONG_NAME, Planner, RowPlan, Table, referenced_objects};
use super::harness::bucket_for;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use scylla::value::CqlValue;
use std::collections::BTreeSet;
use thoradm::backup::coverage::{Registry, Store};
use thorium::Conf;

/// The content type Thorium sets on the objects it uploads
pub const THORIUM_CONTENT_TYPE: &str = "application/octet-stream";

/// The object sizes every layout needs: empty, one byte, and either side of the part size
#[must_use]
pub fn boundary_sizes() -> [u64; 5] {
    // the sizes where single and multipart uploads meet
    [0, 1, PART_SIZE - 1, PART_SIZE, PART_SIZE + 1]
}

/// The prefix layout of the results bucket's entity blobs
pub const ENTITY_BLOB_LAYOUT: &str = "{id}/__thorium_entities/{kind}.json";

/// The content type the API stores a PNG graphic with, which the fixture's graphics carry
pub const GRAPHIC_CONTENT_TYPE: &str = "image/png";

/// Returns the content types a layout's objects carry when Thorium uploaded them
///
/// Graphics keep the image type they were uploaded with. Entity blobs are uploaded with
/// [`THORIUM_CONTENT_TYPE`], which the test cluster's object store replaces with the type of the
/// key's `.json` extension.
///
/// # Arguments
///
/// * `role` - The bucket role
/// * `prefix` - The layout's prefix, or `None` for the whole bucket
#[must_use]
pub fn layout_content_types(role: &str, prefix: Option<&str>) -> &'static [&'static str] {
    // every other layout keeps the content type Thorium sets
    match (role, prefix) {
        ("graphics", _) => &[GRAPHIC_CONTENT_TYPE],
        (_, Some(ENTITY_BLOB_LAYOUT)) => &[THORIUM_CONTENT_TYPE, "application/json"],
        _ => &[THORIUM_CONTENT_TYPE],
    }
}

/// Returns every backed-up layout: its bucket role and the prefix it covers
#[must_use]
pub fn layouts() -> Vec<(String, Option<String>)> {
    // one layout per prefix of a bucket backed up by prefix, and one per other backed-up bucket
    Registry::backed_up(Store::S3)
        .flat_map(|entry| {
            let prefixes: Vec<Option<String>> = if entry.prefixes.is_empty() {
                vec![None]
            } else {
                entry
                    .prefixes
                    .iter()
                    .map(|prefix| Some((*prefix).to_owned()))
                    .collect()
            };
            prefixes
                .into_iter()
                .map(|prefix| (entry.name.to_owned(), prefix))
        })
        .collect()
}

/// Returns whether an object of a bucket belongs to one of its layouts: entity blobs to the
/// entity blob layout, and every other key to the bucket's other layout
///
/// # Arguments
///
/// * `prefix` - The layout's prefix, or `None` for the whole bucket
/// * `key` - The object's key
#[must_use]
pub fn in_layout(prefix: Option<&str>, key: &str) -> bool {
    // entity blobs sit in a fixed directory under their result's id
    let blob = key.split('/').nth(1) == Some("__thorium_entities");
    match prefix {
        Some(ENTITY_BLOB_LAYOUT) => blob,
        _ => !blob,
    }
}

/// Returns the classes a layout needs
///
/// # Arguments
///
/// * `role` - The bucket role
/// * `prefix` - The layout's prefix, or `None` for the whole bucket
#[must_use]
pub fn layout_classes(role: &str, prefix: Option<&str>) -> Vec<ValueClass> {
    // every layout needs the boundary sizes, a multipart object, every byte, and Thorium's metadata
    let mut classes: Vec<ValueClass> = boundary_sizes()
        .into_iter()
        .map(ValueClass::Bytes)
        .collect();
    classes.extend([
        ValueClass::Multipart,
        ValueClass::AllBytes,
        ValueClass::Metadata,
    ]);
    // only result files are named freely, so only they can have odd and near-limit keys
    if role == "results" && prefix != Some(ENTITY_BLOB_LAYOUT) {
        classes.extend([ValueClass::OddKey, ValueClass::Large]);
    }
    classes
}

/// Adds every cell the backed-up layouts need to a matrix
///
/// # Arguments
///
/// * `matrix` - The matrix to add cells to
pub fn require(matrix: &mut CoverageMatrix) {
    // each layout needs each of its classes
    for (role, prefix) in layouts() {
        for class in layout_classes(&role, prefix.as_deref()) {
            matrix.require(
                Location::Layout {
                    bucket: role.clone(),
                    prefix: prefix.clone(),
                },
                class,
                Need::Required,
            );
        }
    }
}

/// Returns the body an object of a class gets
///
/// # Arguments
///
/// * `class` - The class
fn body_for(class: &ValueClass) -> Body {
    // sizes are filled with a readable byte; the others have their own bodies
    match class {
        ValueClass::Bytes(size) => Body::Bytes(vec![b'j'; usize::try_from(*size).unwrap_or(0)]),
        ValueClass::Multipart => Body::Multipart(3 * PART_SIZE + 1),
        ValueClass::AllBytes => Body::Bytes((0..=255).collect()),
        _ => Body::Bytes(b"an ordinary fixture object".to_vec()),
    }
}

/// Returns whether an object class exercises sizes rather than a layout: a body at the part size
/// or uploaded in parts, or a near-limit key
///
/// # Arguments
///
/// * `class` - The class
#[must_use]
pub fn exercises_size(class: &ValueClass) -> bool {
    // the boundary sizes around the part size and the classes built for length
    match class {
        ValueClass::Bytes(size) => *size >= PART_SIZE - 1,
        ValueClass::Multipart | ValueClass::Large => true,
        _ => false,
    }
}

/// Plans one row, with its object, for every class of every layout a row names
///
/// # Arguments
///
/// * `planner` - The planner that builds the rows
/// * `tables` - The live tables of the rows that name objects
/// * `sizes` - Whether to plan the classes that exercise sizes: objects at the part size, uploaded
///   in parts, or under near-limit keys
#[must_use]
pub fn plan(planner: &mut Planner<'_>, tables: &[Table], sizes: bool) -> Vec<RowPlan> {
    // build a row that names one object of each class, in the table that names that layout
    let table = |name: &str| {
        tables
            .iter()
            .find(|table| table.name == name)
            .unwrap_or_else(|| panic!("no live table {name}"))
    };
    let mut plans = Vec::new();
    for (role, prefix) in layouts().into_iter().filter(|(role, _)| role != "graphics") {
        for class in layout_classes(&role, prefix.as_deref())
            .into_iter()
            .filter(|class| sizes || !exercises_size(class))
        {
            let mut values = match role.as_str() {
                "files" | "repos" => {
                    let mut values = planner.base_row(table("s3_ids"));
                    let kind = if role == "repos" { "Repo" } else { "File" };
                    values.insert("type".to_owned(), CqlValue::Text(kind.to_owned()));
                    values
                }
                "attachments" => {
                    let mut values = planner.base_row(table("comments"));
                    let files =
                        serde_json::json!({ "notes.txt": uuid::Uuid::new_v4() }).to_string();
                    values.insert("files".to_owned(), CqlValue::Text(files));
                    values
                }
                "results" if prefix.as_deref() == Some(ENTITY_BLOB_LAYOUT) => {
                    // a result that found one kind of entity names that kind's blob
                    let results = table("results");
                    let mut values = planner.base_row(results);
                    let entities = planner
                        .value(results, "entities", &ValueClass::One)
                        .expect("the fixture builds an entity count");
                    values.insert("entities".to_owned(), entities);
                    values
                }
                "results" => {
                    let mut values = planner.base_row(table("results"));
                    let name = match &class {
                        ValueClass::OddKey => odd_key("layout"),
                        ValueClass::Large => super::columns::long_name("layout"),
                        other => format!("layout-{other}.bin"),
                    };
                    values.insert(
                        "files".to_owned(),
                        CqlValue::Set(vec![CqlValue::Text(name)]),
                    );
                    values
                }
                other => panic!("no table names objects in the {other} bucket"),
            };
            let table_name = match role.as_str() {
                "files" | "repos" => "s3_ids",
                "attachments" => "comments",
                _ => "results",
            };
            planner.derive(table_name, &mut values);
            plans.push(RowPlan {
                table: table_name.to_owned(),
                values,
                body: Some(body_for(&class)),
            });
        }
    }
    plans
}

/// Returns the objects of the graphics bucket's layout: one for every class it needs
///
/// The graphics bucket is copied by listing it, so no row names these objects.
///
/// # Arguments
///
/// * `sizes` - Whether to include the classes that exercise sizes
#[must_use]
pub fn graphics_objects(sizes: bool) -> Vec<(String, Body)> {
    // one object per class, keyed by the class
    layout_classes("graphics", None)
        .into_iter()
        .filter(|class| sizes || !exercises_size(class))
        .map(|class| (format!("fixture/layout-{class}.png"), body_for(&class)))
        .collect()
}

/// Uploads every object of the graphics bucket's layout
///
/// # Arguments
///
/// * `s3` - The S3 client
/// * `conf` - The config naming the buckets
/// * `sizes` - Whether to include the classes that exercise sizes
pub async fn write_graphics(s3: &S3Client, conf: &Conf, sizes: bool) {
    // only a config whose registry backs up the graphics bucket needs its layout
    if !layouts().iter().any(|(role, _)| role == "graphics") {
        return;
    }
    let bucket = bucket_for(conf, "graphics");
    for (key, body) in graphics_objects(sizes) {
        put_typed(s3, &bucket, &key, &body, GRAPHIC_CONTENT_TYPE).await;
    }
}

/// Uploads one object in one request or in parts, with the content type Thorium sets
///
/// # Arguments
///
/// * `s3` - The S3 client
/// * `bucket` - The bucket
/// * `key` - The object's key
/// * `body` - What it holds
///
/// # Panics
///
/// Panics if S3 refuses the upload.
pub async fn put(s3: &S3Client, bucket: &str, key: &str, body: &Body) {
    // Thorium uploads most objects as plain bytes
    put_typed(s3, bucket, key, body, THORIUM_CONTENT_TYPE).await;
}

/// Uploads one object in one request or in parts, with a content type
///
/// # Arguments
///
/// * `s3` - The S3 client
/// * `bucket` - The bucket
/// * `key` - The object's key
/// * `body` - What it holds
/// * `content_type` - The object's content type
///
/// # Panics
///
/// Panics if S3 refuses the upload.
pub async fn put_typed(s3: &S3Client, bucket: &str, key: &str, body: &Body, content_type: &str) {
    // small bodies go in one request
    match body {
        Body::Bytes(bytes) => {
            s3.put_object()
                .bucket(bucket)
                .key(key)
                .content_type(content_type)
                .body(ByteStream::from(bytes.clone()))
                .send()
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "failed to put {bucket}/{key}: {}",
                        aws_sdk_s3::error::DisplayErrorContext(err)
                    )
                });
        }
        Body::Multipart(total) => {
            // larger bodies go in full parts and a short last part
            let upload = s3
                .create_multipart_upload()
                .bucket(bucket)
                .key(key)
                .content_type(content_type)
                .send()
                .await
                .expect("failed to start a multipart upload");
            let id = upload.upload_id().expect("an upload id").to_owned();
            let mut parts = Vec::new();
            let mut sent = 0;
            let mut number = 1;
            while sent < *total {
                let len = PART_SIZE.min(total - sent);
                let part = s3
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(&id)
                    .part_number(number)
                    .body(ByteStream::from(vec![
                        b'p';
                        usize::try_from(len).unwrap_or(0)
                    ]))
                    .send()
                    .await
                    .expect("failed to upload a part");
                parts.push(
                    CompletedPart::builder()
                        .e_tag(part.e_tag().unwrap_or_default())
                        .part_number(number)
                        .build(),
                );
                sent += len;
                number += 1;
            }
            s3.complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .expect("failed to complete a multipart upload");
        }
    }
}

/// Uploads every object the planned rows name
///
/// # Arguments
///
/// * `s3` - The S3 client
/// * `conf` - The config naming the buckets
/// * `plans` - The planned rows
pub async fn write_objects(s3: &S3Client, conf: &Conf, plans: &[RowPlan]) {
    // name each row's objects and give them the row's body or an ordinary one
    let mut written = BTreeSet::new();
    for plan in plans {
        let body = plan
            .body
            .clone()
            .unwrap_or_else(|| Body::Bytes(b"an ordinary fixture object".to_vec()));
        for (role, key) in referenced_objects(plan) {
            let bucket = bucket_for(conf, role);
            if written.insert((bucket.clone(), key.clone())) {
                put(s3, &bucket, &key, &body).await;
            }
        }
    }
}

/// Counts the classes of every object in the backed-up buckets
///
/// # Arguments
///
/// * `s3` - The S3 client
/// * `conf` - The config naming the buckets
/// * `matrix` - The matrix to count observations in
///
/// # Panics
///
/// Panics if a bucket can't be listed or an object read.
pub async fn observe(s3: &S3Client, conf: &Conf, matrix: &mut CoverageMatrix) {
    // list each backed-up layout's bucket
    for (role, prefix) in layouts() {
        let bucket = bucket_for(conf, &role);
        let location = Location::Layout {
            bucket: role.clone(),
            prefix: prefix.clone(),
        };
        let mut pages = s3.list_objects_v2().bucket(&bucket).into_paginator().send();
        while let Some(page) = pages.next().await {
            let page = page.expect("failed to list a bucket");
            for object in page.contents() {
                let key = object.key().unwrap_or_default().to_owned();
                // a bucket backed up by prefix counts each object in its own layout
                if !in_layout(prefix.as_deref(), &key) {
                    continue;
                }
                let size = u64::try_from(object.size().unwrap_or(0)).unwrap_or(0);
                // sizes at the boundaries
                if boundary_sizes().contains(&size) {
                    matrix.observe(location.clone(), ValueClass::Bytes(size));
                }
                // multipart uploads carry the part count in their ETag
                if object.e_tag().unwrap_or_default().contains('-') {
                    matrix.observe(location.clone(), ValueClass::Multipart);
                }
                // a 256-byte object may hold every byte
                if size == 256 {
                    let body = s3
                        .get_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await
                        .expect("failed to read an object");
                    let bytes = body
                        .body
                        .collect()
                        .await
                        .expect("failed to read a body")
                        .into_bytes();
                    if bytes.iter().copied().eq(0..=255) {
                        matrix.observe(location.clone(), ValueClass::AllBytes);
                    }
                }
                // the content type Thorium's uploads carry in this layout
                let head = s3
                    .head_object()
                    .bucket(&bucket)
                    .key(&key)
                    .send()
                    .await
                    .expect("failed to head an object");
                if head.content_type().is_some_and(|found| {
                    layout_content_types(&role, prefix.as_deref()).contains(&found)
                }) {
                    matrix.observe(location.clone(), ValueClass::Metadata);
                }
                // freely named result files
                if role == "results" && prefix.as_deref() != Some(ENTITY_BLOB_LAYOUT) {
                    let name = key.split_once('/').map_or(key.as_str(), |(_, name)| name);
                    if is_odd_key(name) {
                        matrix.observe(location.clone(), ValueClass::OddKey);
                    }
                    if key.len() >= LONG_NAME && key.len() <= MAX_S3_KEY {
                        matrix.observe(location.clone(), ValueClass::Large);
                    }
                }
            }
        }
    }
}
