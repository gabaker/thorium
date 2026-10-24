//! Redis records: `Backup` documents generated from the schema, written through the API's own
//! record writers, and observed from the backup the API returns and from the raw hashes
//!
//! Each flavor's document becomes one group with its users, images, and pipelines, the users'
//! scoped tokens, and the groups' network-policy sets. Records are
//! written the way the API's normal create and update paths write them (never its restore path,
//! which would hide what restore drops); the settings singleton is written and observed once per
//! flavor. Fields only the API's clock can set (`verification_sent`), fields missing from records
//! written before the field existed, and network-policy sets, which the API fills only as images
//! use policies, are written raw.

use super::classes::{CoverageMatrix, Location, ValueClass};
use super::harness::{Harness, redis_pool};
use super::schema::{
    BackupSchema, Flavor, Generator, Node, Shape, flavors, matches_variant, observe,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use thorium::models::backends::db;
use thorium::models::{Backup, Group, Image, Pipeline, ScopedToken, SystemSettings, User};
use thorium::utils::Shared;

/// The Redis family each `Backup` part's records are hashes in, and the part's path
pub const RECORD_FAMILIES: &[(&str, &str, &str)] = &[
    ("users", "user_data", ".users[]"),
    ("groups", "groups_data", ".groups[]"),
    ("images", "image_data", ".images[]"),
    ("pipelines", "pipeline_data", ".pipelines[]"),
];

/// Returns the item node of one of `Backup`'s top-level arrays, or the settings node
///
/// # Arguments
///
/// * `tree` - The walked `Backup` schema
/// * `part` - The top-level property
///
/// # Panics
///
/// Panics if `Backup` has no such property.
#[must_use]
pub fn part_node<'a>(tree: &'a Node, part: &str) -> &'a Node {
    // find the property and unwrap its array
    let Node::Object(props) = tree else {
        panic!("Backup isn't an object");
    };
    let node = &props
        .iter()
        .find(|(name, ..)| name == part)
        .unwrap_or_else(|| panic!("Backup has no {part}"))
        .2;
    match node {
        Node::Array(item) | Node::Set(item) => item,
        other => other,
    }
}

/// The roles a group's members hold
const MEMBER_ROLES: [&str; 4] = ["owners", "managers", "users", "monitors"];

/// Adds copies of a flavor's users until there are as many as its longest group member list
///
/// Each copy takes the identifiers of the user it copies with a suffix, so no two users share a
/// name, email, or token, and it has no aliases, which map one alias to one user.
///
/// # Arguments
///
/// * `doc` - The generated `Backup` document
fn add_member_users(doc: &mut Value) {
    // the most distinct members any one list needs
    let needed = doc["groups"].as_array().map_or(0, |groups| {
        groups
            .iter()
            .flat_map(|group| {
                MEMBER_ROLES.iter().flat_map(move |role| {
                    ["combined", "direct"]
                        .iter()
                        .filter_map(move |list| group[*role][*list].as_array().map(Vec::len))
                })
            })
            .max()
            .unwrap_or(0)
    });
    let Some(users) = doc["users"].as_array_mut() else {
        return;
    };
    // a flavor without users keeps its member lists empty
    let generated = users.len();
    if generated == 0 {
        return;
    }
    for index in generated..needed {
        let mut copy = users[index % generated].clone();
        for field in ["username", "email", "token"] {
            if let Some(text) = copy[field].as_str() {
                copy[field] = Value::String(format!("{text}-member{index}"));
            }
        }
        copy["aliases"] = Value::Object(serde_json::Map::new());
        users.push(copy);
    }
}

/// Gives one flavor's records names that tie them together: images and pipelines in the first
/// group, every pipeline stage running the first image, every user in every group, and only users
/// as group members
///
/// The API only lets users be members of a group, and keeps each member's groups in a set of its
/// own, so a member that isn't a user would leave behind a set no backup carries.
/// # Arguments
///
/// * `doc` - The generated `Backup` document
/// * `flavor` - The flavor, whose empty collections stay empty
fn link(doc: &mut Value, flavor: &Flavor) {
    // the first group owns everything in the document
    let group = doc["groups"][0]["name"].clone();
    let images = doc["images"]
        .as_array_mut()
        .map_or_else(Vec::new, std::mem::take);
    let mut linked_images = Vec::new();
    for mut image in images {
        image["group"] = group.clone();
        image["used_by"] = Value::Array(Vec::new());
        linked_images.push(image);
    }
    let first_image = linked_images.first().map(|image| image["name"].clone());
    doc["images"] = Value::Array(linked_images);
    // pipeline stages keep their generated shape but all run the first image
    if let Some(pipelines) = doc["pipelines"].as_array_mut() {
        for pipeline in pipelines {
            pipeline["group"] = group.clone();
            if let (Some(image), Some(stages)) = (&first_image, pipeline["order"].as_array_mut()) {
                for stage in stages.iter_mut().filter_map(Value::as_array_mut) {
                    for name in stage {
                        *name = image.clone();
                    }
                }
            }
        }
    }
    // every generated member becomes a distinct user of the flavor, keeping each list's size
    add_member_users(doc);
    let usernames: Vec<Value> = doc["users"].as_array().map_or_else(Vec::new, |users| {
        users.iter().map(|user| user["username"].clone()).collect()
    });
    link_tokens_and_sets(doc, &usernames);
    let Some(groups) = doc["groups"].as_array_mut() else {
        return;
    };
    for group in groups {
        for role in MEMBER_ROLES {
            for list in ["combined", "direct"] {
                if let Some(members) = group[role][list].as_array_mut() {
                    let size = members.len();
                    *members = usernames.iter().take(size).cloned().collect();
                }
            }
        }
        // users join every group as plain users, unless the flavor wants empty member lists
        if flavor.size == 0 {
            continue;
        }
        for list in ["combined", "direct"] {
            if let Some(members) = group["users"][list].as_array_mut() {
                members.extend(usernames.iter().cloned());
            }
        }
    }
}

/// Ties a flavor's scoped tokens to its users and its network-policy sets to its groups
///
/// Each scoped token belongs to one of the flavor's users in turn, and a developer token gets a
/// copy with each of its flags flipped, so both values of every flag are stored; a flavor without
/// users keeps none. Each network-policy set belongs to one of the flavor's groups, a set beyond the last
/// group or without policies is dropped, and the first group's set also holds every policy its
/// images use, as the API's image writes would have added them.
///
/// # Arguments
///
/// * `doc` - The generated `Backup` document
/// * `usernames` - The flavor's usernames
fn link_tokens_and_sets(doc: &mut Value, usernames: &[Value]) {
    // each token's owner is a user of the flavor
    let tokens = doc["scoped_tokens"]
        .as_array_mut()
        .map_or_else(Vec::new, std::mem::take);
    let mut linked_tokens = Vec::new();
    if !usernames.is_empty() {
        for (index, mut token) in tokens.into_iter().enumerate() {
            token["owner"] = usernames[index % usernames.len()].clone();
            let flipped = flipped_developer(&token);
            linked_tokens.push(token);
            linked_tokens.extend(flipped);
        }
    }
    doc["scoped_tokens"] = Value::Array(linked_tokens);
    // each set belongs to its own group
    let groups: Vec<Value> = doc["groups"].as_array().map_or_else(Vec::new, |groups| {
        groups.iter().map(|group| group["name"].clone()).collect()
    });
    let sets = doc["network_policy_sets"]
        .as_array_mut()
        .map_or_else(Vec::new, std::mem::take);
    let mut linked_sets: Vec<Value> = sets
        .into_iter()
        .zip(groups.iter())
        .map(|(mut set, group)| {
            set["group"] = group.clone();
            set
        })
        .collect();
    // the first group's set holds the policies its images use
    let used: BTreeSet<String> = doc["images"]
        .as_array()
        .map_or_else(BTreeSet::new, |images| {
            images
                .iter()
                .flat_map(|image| {
                    image["network_policies"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|policy| policy.as_str().map(str::to_owned))
                .collect()
        });
    if let Some(first) = groups.first()
        && !used.is_empty()
    {
        if linked_sets.is_empty() {
            linked_sets.push(serde_json::json!({"group": first, "policies": []}));
        }
        let policies = linked_sets[0]["policies"].as_array_mut();
        if let Some(policies) = policies {
            for policy in used {
                let policy = Value::String(policy);
                if !policies.contains(&policy) {
                    policies.push(policy);
                }
            }
        }
    }
    // a set without policies isn't stored
    linked_sets.retain(|set| {
        set["policies"]
            .as_array()
            .is_some_and(|policies| !policies.is_empty())
    });
    doc["network_policy_sets"] = Value::Array(linked_sets);
}

/// Returns a copy of a developer scoped token with each flag flipped, under its own name and
/// value, or `None` for a token of another role
///
/// # Arguments
///
/// * `token` - The generated scoped token
fn flipped_developer(token: &Value) -> Option<Value> {
    // only a developer role carries flags
    let flags = token["role"].get("Developer")?.as_object()?;
    let mut copy = token.clone();
    copy["role"]["Developer"] = Value::Object(
        flags
            .iter()
            .map(|(flag, value)| (flag.clone(), Value::Bool(!value.as_bool().unwrap_or(false))))
            .collect(),
    );
    // a name and value of its own, as each token's are unique
    for field in ["name", "token"] {
        if let Some(text) = copy[field].as_str() {
            copy[field] = Value::String(format!("{text}-flipped"));
        }
    }
    Some(copy)
}

/// Writes one flavor's records through the API's record writers
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `shared` - The API's shared state for this namespace
/// * `doc` - The flavor's linked `Backup` document
/// * `flavor` - The flavor
///
/// # Panics
///
/// Panics if a record doesn't decode or a writer fails, naming the flavor.
async fn write(harness: &Harness, shared: &Shared, doc: &Value, flavor: &Flavor) {
    // decode each part into the API's own types, which is what its writers take
    let decode = |part: &str| -> Vec<Value> { doc[part].as_array().cloned().unwrap_or_default() };
    let label = &flavor.label;
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let ns = &harness.conf.thorium.namespace;
    // groups: join the group set, then write roles, allowed, and description
    for raw in decode("groups") {
        let group: Group =
            serde_json::from_value(raw).unwrap_or_else(|err| panic!("{label} group: {err}"));
        let _: () = redis::cmd("SADD")
            .arg(format!("{ns}:groups"))
            .arg(&group.name)
            .query_async(&mut *conn)
            .await
            .unwrap();
        let members: HashSet<String> = [
            &group.owners,
            &group.managers,
            &group.users,
            &group.monitors,
        ]
        .iter()
        .flat_map(|role| role.combined.iter().cloned())
        .collect();
        db::groups::update(&group, &members, &HashSet::new(), shared)
            .await
            .unwrap_or_else(|err| panic!("{label} group write: {err:?}"));
    }
    // users: create, then save, and set the times only the API's clock writes
    for raw in decode("users") {
        let user: User =
            serde_json::from_value(raw).unwrap_or_else(|err| panic!("{label} user: {err}"));
        db::users::create(user.clone(), shared)
            .await
            .unwrap_or_else(|err| panic!("{label} user create: {err:?}"));
        db::users::save(&user, shared)
            .await
            .unwrap_or_else(|err| panic!("{label} user save: {err:?}"));
        if let Some(sent) = &user.verification_sent {
            let _: () = redis::cmd("HSET")
                .arg(format!("{ns}:user_data:{}", user.username))
                .arg("verification_sent")
                .arg(serde_json::to_string(sent).unwrap())
                .query_async(&mut *conn)
                .await
                .unwrap();
        }
    }
    // images: create, then update, which writes bans and everything create leaves out
    for raw in decode("images") {
        let image: Image =
            serde_json::from_value(raw).unwrap_or_else(|err| panic!("{label} image: {err}"));
        let mut pipe = redis::pipe();
        db::images::build(&mut pipe, &image, shared)
            .unwrap_or_else(|err| panic!("{label} image build: {err:?}"));
        let _: () = pipe.atomic().query_async(&mut *conn).await.unwrap();
        db::images::update(&image, shared)
            .await
            .unwrap_or_else(|err| panic!("{label} image update: {err:?}"));
    }
    // pipelines: create, then update, which writes bans
    for raw in decode("pipelines") {
        let pipeline: Pipeline =
            serde_json::from_value(raw).unwrap_or_else(|err| panic!("{label} pipeline: {err}"));
        let mut pipe = redis::pipe();
        db::pipelines::build(&mut pipe, &pipeline, shared)
            .unwrap_or_else(|err| panic!("{label} pipeline build: {err:?}"));
        let _: () = pipe.atomic().query_async(&mut *conn).await.unwrap();
        db::pipelines::update(&pipeline, &[], &[], shared)
            .await
            .unwrap_or_else(|err| panic!("{label} pipeline update: {err:?}"));
    }
    // scoped tokens: create
    for raw in decode("scoped_tokens") {
        let token: ScopedToken =
            serde_json::from_value(raw).unwrap_or_else(|err| panic!("{label} scoped token: {err}"));
        db::users::create_scoped_token(&token, shared)
            .await
            .unwrap_or_else(|err| panic!("{label} scoped token create: {err:?}"));
    }
    // network-policy sets: add each policy to its group's set
    for set in decode("network_policy_sets") {
        let group = set["group"].as_str().unwrap_or_default();
        for policy in set["policies"].as_array().cloned().unwrap_or_default() {
            let _: () = redis::cmd("SADD")
                .arg(format!("{ns}:netpols:{group}"))
                .arg(policy.as_str().unwrap_or_default())
                .query_async(&mut *conn)
                .await
                .unwrap();
        }
    }
}

/// Removes the optional keys at one object depth from one record's hash, as data written before
/// those keys existed would look
///
/// # Arguments
///
/// * `conn` - A Redis connection
/// * `key` - The record's hash key
/// * `node` - The record's schema node
/// * `depth` - The object depth to strip; zero removes the optional hash fields themselves
async fn strip_optional(
    conn: &mut redis::aio::MultiplexedConnection,
    key: &str,
    node: &Node,
    depth: usize,
) {
    // read the record
    let Node::Object(props) = node else {
        return;
    };
    let hash: BTreeMap<String, String> = redis::cmd("HGETALL")
        .arg(key)
        .query_async(conn)
        .await
        .unwrap();
    for (field, stored) in hash {
        let Some((_, required, prop)) = props.iter().find(|(name, ..)| *name == field) else {
            continue;
        };
        // at depth zero an optional field is left out entirely
        if depth == 0 {
            if !required {
                let _: () = redis::cmd("HDEL")
                    .arg(key)
                    .arg(&field)
                    .query_async(conn)
                    .await
                    .unwrap();
            }
            continue;
        }
        // deeper, a JSON field loses the optional keys at that depth
        if let Ok(mut value) = serde_json::from_str::<Value>(&stored)
            && (value.is_object() || value.is_array())
        {
            drop_optional_keys(&mut value, prop, depth - 1);
            let _: () = redis::cmd("HSET")
                .arg(key)
                .arg(&field)
                .arg(value.to_string())
                .query_async(conn)
                .await
                .unwrap();
        }
    }
}

/// Removes the optional keys of the objects at one depth of a JSON value
///
/// # Arguments
///
/// * `value` - The JSON value
/// * `node` - Its schema node
/// * `depth` - How many more object levels to descend before removing keys
fn drop_optional_keys(value: &mut Value, node: &Node, depth: usize) {
    // objects at the depth lose their optional keys; shallower ones pass the walk on
    match (node, value) {
        (Node::Object(props), Value::Object(map)) => {
            for (name, required, prop) in props {
                if depth == 0 && !required {
                    map.remove(name);
                } else if depth > 0
                    && let Some(inner) = map.get_mut(name)
                {
                    drop_optional_keys(inner, prop, depth - 1);
                }
            }
        }
        (Node::Array(item) | Node::Set(item), Value::Array(items)) => {
            for inner in items {
                drop_optional_keys(inner, item, depth);
            }
        }
        (Node::Map(item), Value::Object(map)) => {
            for inner in map.values_mut() {
                drop_optional_keys(inner, item, depth);
            }
        }
        (Node::OneOf(alternatives), value) => {
            if let Some((_, alternative)) = alternatives
                .iter()
                .find(|(label, alternative)| matches_variant(label, alternative, value))
            {
                drop_optional_keys(value, alternative, depth);
            }
        }
        _ => (),
    }
}

/// Counts optional fields and keys missing from the raw records of one family
///
/// # Arguments
///
/// * `conn` - A Redis connection
/// * `keys` - The records' hash keys, including records whose every field was removed
/// * `path` - The `Backup` path of the records
/// * `node` - The records' schema node
/// * `matrix` - The matrix to count observations in
async fn observe_absent(
    conn: &mut redis::aio::MultiplexedConnection,
    keys: &BTreeSet<String>,
    path: &str,
    node: &Node,
    matrix: &mut CoverageMatrix,
) {
    // a missing optional field is absent; a stored one is searched for missing keys
    let Node::Object(props) = node else {
        return;
    };
    for key in keys {
        let hash: BTreeMap<String, String> = redis::cmd("HGETALL")
            .arg(key)
            .query_async(conn)
            .await
            .unwrap_or_default();
        for (name, required, prop) in props {
            let child = format!("{path}.{name}");
            match hash.get(name) {
                None if !required => {
                    matrix.observe(Location::Field { path: child }, ValueClass::Absent);
                }
                Some(raw) => {
                    if let Ok(value) = serde_json::from_str::<Value>(raw) {
                        observe_absent_keys(&value, prop, &child, matrix);
                    }
                }
                None => (),
            }
        }
    }
}

/// Counts optional keys missing inside a stored JSON value
///
/// # Arguments
///
/// * `value` - The stored JSON value
/// * `node` - Its schema node
/// * `path` - Its path
/// * `matrix` - The matrix to count observations in
fn observe_absent_keys(value: &Value, node: &Node, path: &str, matrix: &mut CoverageMatrix) {
    // objects report missing optional keys; containers pass the walk on
    match (node, value) {
        (Node::Object(props), Value::Object(map)) => {
            for (name, required, prop) in props {
                let child = format!("{path}.{name}");
                match map.get(name) {
                    None if !required => {
                        matrix.observe(Location::Field { path: child }, ValueClass::Absent);
                    }
                    Some(inner) => observe_absent_keys(inner, prop, &child, matrix),
                    None => (),
                }
            }
        }
        (Node::Array(item) | Node::Set(item), Value::Array(items)) => {
            for inner in items {
                observe_absent_keys(inner, item, &format!("{path}[]"), matrix);
            }
        }
        (Node::Map(item), Value::Object(map)) => {
            for inner in map.values() {
                observe_absent_keys(inner, item, &format!("{path}{{}}"), matrix);
            }
        }
        (Node::OneOf(alternatives), value) => {
            if let Some((label, alternative)) = alternatives
                .iter()
                .find(|(label, alternative)| matches_variant(label, alternative, value))
            {
                observe_absent_keys(value, alternative, &format!("{path}<{label}>"), matrix);
            }
        }
        _ => (),
    }
}

/// Seeds every flavor's records, counts the classes the namespace's Redis state holds, and returns
/// every derived field that disagrees with its source
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `schema` - The `Backup` schema
/// * `matrix` - The matrix to count observations in
/// * `sizes` - Whether to write the flavors that exercise sizes: large values and many elements
///
/// # Panics
///
/// Panics if a record can't be written or the backup can't be read.
#[must_use]
pub async fn seed_and_observe(
    harness: &Harness,
    schema: &BackupSchema,
    matrix: &mut CoverageMatrix,
    sizes: bool,
) -> Vec<String> {
    // connect the API's record writers to this namespace
    let shared = Box::pin(Shared::with_opts(
        harness.conf.clone(),
        thorium::utils::StartupOpts { apply_schema: true },
    ))
    .await
    .expect("the API starts and applies the schema");
    let client = harness.client().await;
    let tree = schema.tree();
    let settings_node = part_node(&tree, "settings").clone();
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut raw = pool.dedicated_connection().await.unwrap();
    let ns = harness.conf.thorium.namespace.clone();
    let mut stripped: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    // write each flavor's document
    for flavor in flavors(Shape::of(&tree))
        .into_iter()
        .filter(|flavor| sizes || (flavor.scalar != Some(ValueClass::Large) && flavor.size <= 1))
    {
        let mut doc = Generator::new(&flavor).value(&tree, "", 0);
        link(&mut doc, &flavor);
        write(harness, &shared, &doc, &flavor).await;
        // the settings singleton holds one flavor at a time, so it is observed per flavor
        let settings: SystemSettings = serde_json::from_value(doc["settings"].clone())
            .unwrap_or_else(|err| panic!("{} settings: {err}", flavor.label));
        db::system::update_settings(&settings, &shared)
            .await
            .unwrap_or_else(|err| panic!("{} settings write: {err:?}", flavor.label));
        let stored = client
            .system
            .get_settings()
            .await
            .expect("failed to read settings");
        observe(
            &settings_node,
            &serde_json::to_value(&stored).unwrap(),
            ".settings",
            matrix,
        );
        // records of an absent flavor lose their optional keys at one depth, as older data would
        let Some(depth) = flavor.absent else {
            continue;
        };
        for (part, family, _) in RECORD_FAMILIES {
            let node = part_node(&tree, part).clone();
            for record in doc[*part].as_array().cloned().unwrap_or_default() {
                let key = record_key(&ns, family, &record);
                strip_optional(&mut raw, &key, &node, depth).await;
                stripped.entry(*family).or_default().insert(key);
            }
        }
        let settings_key = format!("{ns}:system_settings");
        strip_optional(&mut raw, &settings_key, &settings_node, depth).await;
        let stored: BTreeMap<String, String> = redis::cmd("HGETALL")
            .arg(&settings_key)
            .query_async(&mut raw)
            .await
            .unwrap();
        let as_json: serde_json::Map<String, Value> = stored
            .into_iter()
            .map(|(field, value)| {
                (
                    field,
                    serde_json::from_str(&value).unwrap_or(Value::String(value)),
                )
            })
            .collect();
        observe_absent_keys(&Value::Object(as_json), &settings_node, ".settings", matrix);
        // the API can't read settings missing a field, so write them back whole
        db::system::update_settings(&settings, &shared)
            .await
            .unwrap_or_else(|err| panic!("settings rewrite: {err:?}"));
    }
    // count every class the API's backup document holds
    let backup = client
        .system
        .backup()
        .await
        .expect("failed to read the Redis backup");
    let doc = serde_json::to_value(&backup).expect("the backup serializes");
    observe_backup(&tree, &doc, matrix);
    // count optional fields missing from the raw records, including records left with no fields
    for (part, family, path) in RECORD_FAMILIES {
        let node = part_node(&tree, part).clone();
        let mut keys: BTreeSet<String> =
            super::harness::scan_keys(&mut raw, &format!("{ns}:{family}:*"))
                .await
                .unwrap()
                .into_iter()
                .collect();
        keys.extend(stripped.remove(family).unwrap_or_default());
        observe_absent(&mut raw, &keys, path, &node, matrix).await;
    }
    derived_problems(&doc)
}

/// Counts every class a backup document holds, and the parts a backup written before an optional
/// part existed reads as empty
///
/// # Arguments
///
/// * `tree` - The walked `Backup` schema
/// * `doc` - The backup document the API returned
/// * `matrix` - The matrix to count observations in
///
/// # Panics
///
/// Panics if the document without one of its optional parts doesn't parse.
fn observe_backup(tree: &Node, doc: &Value, matrix: &mut CoverageMatrix) {
    // the settings singleton is observed per flavor, so every other part is observed here
    let Node::Object(props) = tree else {
        return;
    };
    for (name, required, node) in props {
        if name != "settings" {
            observe(node, &doc[name.as_str()], &format!(".{name}"), matrix);
        }
        // a backup written before an optional part existed reads it as empty
        if !required {
            let mut older = doc.clone();
            if let Some(fields) = older.as_object_mut() {
                fields.remove(name.as_str());
            }
            let parsed: Backup = serde_json::from_value(older)
                .unwrap_or_else(|err| panic!("a backup without {name} doesn't parse: {err}"));
            let read = serde_json::to_value(&parsed).expect("the backup serializes");
            if read[name.as_str()] == Value::Array(Vec::new()) {
                matrix.observe(
                    Location::Field {
                        path: format!(".{name}"),
                    },
                    ValueClass::Absent,
                );
            }
        }
    }
}

/// Checks that every derived `Backup` field agrees with its source
///
/// # Arguments
///
/// * `doc` - The backup document the API returned
#[must_use]
pub fn derived_problems(doc: &Value) -> Vec<String> {
    // each group lists exactly the users whose role is analyst
    let mut problems = Vec::new();
    let empty = Vec::new();
    let users = doc["users"].as_array().unwrap_or(&empty);
    let analysts: BTreeSet<&str> = users
        .iter()
        .filter(|user| user["role"] == "Analyst")
        .filter_map(|user| user["username"].as_str())
        .collect();
    for group in doc["groups"].as_array().unwrap_or(&empty) {
        let listed: BTreeSet<&str> = group["analysts"]
            .as_array()
            .map_or_else(BTreeSet::new, |names| {
                names.iter().filter_map(Value::as_str).collect()
            });
        if listed != analysts {
            problems.push(format!(
                "group {} lists {} analysts but {} users are analysts",
                group["name"],
                listed.len(),
                analysts.len()
            ));
        }
    }
    // backups never hold the groups an admin or analyst is read with
    for user in users {
        if user["actual_groups"]
            .as_array()
            .is_some_and(|groups| !groups.is_empty())
        {
            problems.push(format!(
                "user {} has actual groups in a backup",
                user["username"]
            ));
        }
    }
    problems
}

/// Builds the hash key of one generated record
///
/// # Arguments
///
/// * `ns` - The namespace
/// * `family` - The record's Redis family
/// * `record` - The generated record
fn record_key(ns: &str, family: &str, record: &Value) -> String {
    // images and pipelines are keyed by group and name, users and groups by name
    let text = |field: &str| record[field].as_str().unwrap_or_default().to_owned();
    match family {
        "user_data" => format!("{ns}:user_data:{}", text("username")),
        "groups_data" => format!("{ns}:groups_data:{}", text("name")),
        "image_data" => format!("{ns}:image_data:{}:{}", text("group"), text("name")),
        _ => format!("{ns}:pipeline_data:{}:{}", text("group"), text("name")),
    }
}

/// Hash fields the API stores in a record that its `Backup` schema has no property for, with the
/// reason
pub const UNMAPPED_FIELDS: &[(&str, &str, &str)] = &[(
    "user_data",
    "theme",
    "the flat form of users[].settings.theme, which the oracle folds into settings",
)];

/// Returns every hash field of the backed-up record families that maps to no schema property
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `tree` - The walked `Backup` schema
pub async fn unmapped_fields(harness: &Harness, tree: &Node) -> Vec<String> {
    // read every record of each family and check its fields against the part's properties
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let ns = &harness.conf.thorium.namespace;
    let mut families: Vec<(&str, String, &Node)> = RECORD_FAMILIES
        .iter()
        .map(|(part, family, _)| (*family, format!("{ns}:{family}:*"), part_node(tree, part)))
        .collect();
    families.push((
        "system_settings",
        format!("{ns}:system_settings"),
        part_node(tree, "settings"),
    ));
    let mut unmapped = Vec::new();
    for (family, pattern, node) in families {
        let Node::Object(props) = node else {
            continue;
        };
        let keys: Vec<String> = super::harness::scan_keys(&mut *conn, &pattern)
            .await
            .unwrap();
        for key in keys {
            let fields: Vec<String> = redis::cmd("HKEYS")
                .arg(&key)
                .query_async(&mut *conn)
                .await
                .unwrap_or_default();
            for field in fields {
                let mapped = props.iter().any(|(name, ..)| *name == field)
                    || UNMAPPED_FIELDS
                        .iter()
                        .any(|(listed, name, _)| *listed == family && *name == field);
                if !mapped {
                    unmapped.push(format!(
                        "{family} field {field:?} (in {key}) maps to no Backup property"
                    ));
                }
            }
        }
    }
    unmapped.sort();
    unmapped.dedup_by(|a, b| a.split(" (in ").next() == b.split(" (in ").next());
    unmapped
}

/// Returns how many flavors of records the value-class fixture writes
#[must_use]
pub fn flavor_count() -> usize {
    // every flavor of the schema's shape
    let tree = BackupSchema::load().tree();
    flavors(Shape::of(&tree)).len()
}

/// Writes the records of some of the value-class fixture's flavors, the way the API's own writers
/// store them, with the settings of the last one
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `picks` - The flavors to write, by their place in the flavor list (wrapping around); each
///   is written once
///
/// # Panics
///
/// Panics if a record can't be written.
pub async fn seed_flavors(harness: &Harness, picks: &[usize]) {
    // connect the API's record writers to this namespace
    let shared = Box::pin(Shared::with_opts(
        harness.conf.clone(),
        thorium::utils::StartupOpts { apply_schema: true },
    ))
    .await
    .expect("the API starts and applies the schema");
    let tree = BackupSchema::load().tree();
    let all = flavors(Shape::of(&tree));
    let picks: BTreeSet<usize> = picks.iter().map(|pick| pick % all.len()).collect();
    for pick in picks {
        // each flavor's document, linked and written once
        let flavor = &all[pick];
        let mut doc = Generator::new(flavor).value(&tree, "", 0);
        link(&mut doc, flavor);
        write(harness, &shared, &doc, flavor).await;
        let settings: SystemSettings = serde_json::from_value(doc["settings"].clone())
            .unwrap_or_else(|err| panic!("{} settings: {err}", flavor.label));
        db::system::update_settings(&settings, &shared)
            .await
            .unwrap_or_else(|err| panic!("{} settings write: {err:?}", flavor.label));
    }
}
