//! The deep equality oracle sees every difference it claims to: each location of a seeded
//! namespace's snapshot, changed in a copy, is reported at exactly that location

mod common;

use common::columns::live_table;
use common::fixture;
use common::harness::Harness;
use common::oracle::{
    Difference, EPHEMERAL_FAMILIES, ORACLE_RULES, ObjectRepr, Oracle, OracleRule, OracleRuleKind,
    RedisEntry, RedisValueRepr, Snapshot, is_ephemeral, snapshot_tables, stale_rules,
};
use common::records::{RECORD_FAMILIES, part_node};
use common::schema::{BackupSchema, Node};
use serde_json::Value;
use thoradm::backup::coverage::{Registry, Store};
use tokio::sync::OnceCell;

/// The seeded namespace's snapshot, taken once per binary
static SNAPSHOT: OnceCell<Snapshot> = OnceCell::const_new();

/// Held by tests that snapshot the live namespace again, so one test's extra key never shows in
/// another's snapshot
static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Seeds the fixture and snapshots it
async fn seeded() -> &'static Snapshot {
    // seed once; every test reads the same snapshot
    SNAPSHOT
        .get_or_init(|| async {
            let harness = Harness::get().await;
            fixture::broad(harness).await;
            let _ = Box::pin(fixture::value_classes(harness)).await;
            common::seed::icon(&harness.api_url, &harness.token).await;
            expiring_notification(harness).await;
            let counted = common::cycle::census(harness);
            assert!(
                counted.status.success(),
                "{}",
                common::cycle::describe(&counted)
            );
            Snapshot::take(harness).await
        })
        .await
}

/// Rewrites one notification with a TTL, so the snapshot holds an expiring row
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
async fn expiring_notification(harness: &Harness) {
    // copy a notification back over itself with a day to live
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let (row,): (String,) = scylla
        .query_unpaged(format!("SELECT JSON * FROM {ns}.notifications LIMIT 1"), ())
        .await
        .expect("failed to read a notification")
        .into_rows_result()
        .expect("notifications have rows")
        .first_row()
        .expect("the fixture has a notification");
    scylla
        .query_unpaged(
            format!("INSERT INTO {ns}.notifications JSON ? USING TTL 86400"),
            (row,),
        )
        .await
        .expect("failed to give a notification a TTL");
}

/// Builds an oracle for comparing two snapshots of a namespace no restore touched: the rules
/// that describe what a restore does (resets, absences, format losses, rebuilt sets) don't apply,
/// so those locations compare for plain equality
fn unchanged_oracle() -> Oracle {
    // keep only the rules that hold between any two snapshots
    let rules: Vec<OracleRule> = ORACLE_RULES
        .iter()
        .filter(|rule| {
            !matches!(
                rule.kind,
                OracleRuleKind::Reset(_)
                    | OracleRuleKind::Absent
                    | OracleRuleKind::FormatLoss(_)
                    | OracleRuleKind::Recomputed
            )
        })
        .cloned()
        .collect();
    Oracle {
        rules: Vec::leak(rules),
        format: None,
    }
}

/// Asserts a comparison found exactly one difference, at a location
///
/// # Arguments
///
/// * `differences` - What the oracle found
/// * `location` - Where the one difference must be
fn assert_only(differences: &[Difference], location: &str) {
    // one difference, at the changed location
    let found: Vec<String> = differences.iter().map(ToString::to_string).collect();
    assert_eq!(
        differences.len(),
        1,
        "changing {location} gave:\n{}",
        found.join("\n")
    );
    assert_eq!(differences[0].location, location, "{}", found.join("\n"));
}

/// Returns a changed copy of a rendered value that still differs after any recomputation
///
/// # Arguments
///
/// * `value` - The rendered value
fn changed(value: &str) -> String {
    // a different rendering is enough; snapshots compare renderings
    format!("{value} (changed)")
}

/// Two snapshots of a namespace nothing touched compare equal, including taken again
#[tokio::test]
async fn unchanged_snapshots_compare_equal() {
    // the same snapshot, and a fresh one of the untouched namespace
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    assert_eq!(oracle.compare(snapshot, snapshot), Vec::new());
    let live = LIVE.lock().await;
    let again = Snapshot::take(Harness::get().await).await;
    drop(live);
    let differences: Vec<String> = oracle
        .compare(snapshot, &again)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        differences.is_empty(),
        "an untouched namespace changed:\n{}",
        differences.join("\n")
    );
}

/// Changing one column of one row of any table or view is reported at that row and column
#[tokio::test]
async fn every_column_and_view_change_is_found() {
    // change each set, non-key column of each table and view once
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    let harness = Harness::get().await;
    let scylla = harness.scylla().await;
    let mut unset = Vec::new();
    for name in snapshot_tables() {
        // a table the oracle ignores outright, such as the schema record a restore writes, has
        // no change to find
        if oracle.ignores(name) {
            continue;
        }
        let table = live_table(&scylla, &harness.conf.thorium.namespace, name).await;
        let Some(rows) = snapshot.scylla.get(name) else {
            continue;
        };
        for column in table.columns.iter().filter(|column| !column.is_key()) {
            let Some((key, row)) = rows.iter().find(|(_, row)| row.contains_key(&column.name))
            else {
                unset.push(format!("{name}.{}", column.name));
                continue;
            };
            let mut copy = snapshot.clone();
            let value = changed(&row[&column.name]);
            copy.scylla
                .get_mut(name)
                .unwrap()
                .get_mut(key)
                .unwrap()
                .insert(column.name.clone(), value);
            let location = format!("{name} [{key}] {}", column.name);
            assert_only(&oracle.compare(snapshot, &copy), &location);
        }
    }
    // the fixture sets every non-key column of every backed-up table somewhere
    let backed_up: Vec<&str> = Registry::backed_up(Store::ScyllaTable)
        .map(|entry| entry.name)
        .collect();
    unset.retain(|location| {
        backed_up
            .iter()
            .any(|table| location.starts_with(&format!("{table}.")))
    });
    assert!(unset.is_empty(), "no row sets {unset:?}");
}

/// A missing and an extra row are each reported at that row
#[tokio::test]
async fn missing_and_extra_rows_are_found() {
    // remove one row, then add a made-up one
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    let (table, rows) = snapshot
        .scylla
        .iter()
        .find(|(_, rows)| !rows.is_empty())
        .unwrap();
    let key = rows.keys().next().unwrap().clone();
    let mut missing = snapshot.clone();
    missing.scylla.get_mut(table).unwrap().remove(&key);
    assert_only(
        &oracle.compare(snapshot, &missing),
        &format!("{table} [{key}]"),
    );
    let mut extra = snapshot.clone();
    extra
        .scylla
        .get_mut(table)
        .unwrap()
        .insert("made-up".to_owned(), rows[&key].clone());
    assert_only(
        &oracle.compare(snapshot, &extra),
        &format!("{table} [made-up]"),
    );
}

/// Returns a JSON value with one leaf changed, following a path of keys into it
///
/// # Arguments
///
/// * `value` - The stored JSON value
/// * `node` - Its schema node
fn change_leaf(value: &mut Value, node: &Node) -> bool {
    // descend into the first child that holds a leaf, and change the leaf
    match (node, value) {
        (Node::Object(props), Value::Object(map)) => props.iter().any(|(name, _, prop)| {
            map.get_mut(name)
                .is_some_and(|inner| change_leaf(inner, prop))
        }),
        (Node::Array(item) | Node::Set(item), Value::Array(items)) => {
            items.iter_mut().any(|inner| change_leaf(inner, item))
        }
        (Node::Map(item), Value::Object(map)) => {
            map.values_mut().any(|inner| change_leaf(inner, item))
        }
        (Node::OneOf(alternatives), value) => alternatives
            .iter()
            .any(|(_, alternative)| change_leaf(value, alternative)),
        (_, Value::String(text)) => {
            text.push_str(" (changed)");
            true
        }
        (_, Value::Number(number)) => {
            *number = serde_json::Number::from(424_242);
            true
        }
        (_, Value::Bool(flag)) => {
            *flag = !*flag;
            true
        }
        _ => false,
    }
}

/// Changing any `Backup` field stored in a record's hash is reported at that key and field
#[tokio::test]
async fn every_backup_field_change_is_found() {
    // for each record family and each top-level property, change a leaf in one stored record
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    let tree = BackupSchema::load().tree();
    let mut families: Vec<(&str, &Node)> = RECORD_FAMILIES
        .iter()
        .map(|(part, family, _)| (*family, part_node(&tree, part)))
        .collect();
    families.push(("system_settings", part_node(&tree, "settings")));
    let mut checked = std::collections::BTreeSet::new();
    for (family, node) in families {
        let Node::Object(props) = node else {
            continue;
        };
        for (field, _, prop) in props {
            // fields a rule leaves uncompared can't be found changed
            let entry = if family == "system_settings" {
                family.to_owned()
            } else {
                format!("{family}:*")
            };
            if ORACLE_RULES
                .iter()
                .any(|rule| rule.location == format!("{entry} field {field}"))
            {
                continue;
            }
            // find a record of the family that stores the field with a changeable leaf
            let found = snapshot.redis.iter().find_map(|(key, entry)| {
                let RedisValueRepr::Hash(fields) = &entry.value else {
                    return None;
                };
                let in_family = key == family || key.starts_with(&format!("{family}:"));
                let stored = fields.get(field)?;
                let mut value: Value =
                    serde_json::from_str(stored).unwrap_or(Value::String(stored.clone()));
                (in_family && change_leaf(&mut value, prop)).then(|| (key.clone(), value))
            });
            let Some((key, value)) = found else {
                continue;
            };
            let mut copy = snapshot.clone();
            let RedisValueRepr::Hash(fields) = &mut copy.redis.get_mut(&key).unwrap().value else {
                unreachable!("the record is a hash");
            };
            let text = match value {
                Value::String(text) => text,
                other => other.to_string(),
            };
            fields.insert(field.clone(), text);
            assert_only(
                &oracle.compare(snapshot, &copy),
                &format!("redis {key} field {field}"),
            );
            checked.insert(family);
        }
    }
    // every record family had fields to change
    assert_eq!(
        checked.len(),
        RECORD_FAMILIES.len() + 1,
        "only {checked:?} had fields to change"
    );
}

/// Missing and extra keys, fields, and set members are each reported where they are
#[tokio::test]
async fn missing_and_extra_redis_entries_are_found() {
    // a hash and a set of the namespace's backed-up families
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    let (hash_key, hash) = snapshot
        .redis
        .iter()
        .find(|(key, entry)| !is_ephemeral(key) && matches!(entry.value, RedisValueRepr::Hash(_)))
        .unwrap();
    let (set_key, _) = snapshot
        .redis
        .iter()
        .find(|(key, entry)| !is_ephemeral(key) && matches!(entry.value, RedisValueRepr::Set(_)))
        .unwrap();
    // a missing key and an extra key
    let mut copy = snapshot.clone();
    copy.redis.remove(hash_key);
    assert_only(
        &oracle.compare(snapshot, &copy),
        &format!("redis {hash_key}"),
    );
    let mut copy = snapshot.clone();
    copy.redis
        .insert("groups_data:made-up".to_owned(), hash.clone());
    assert_only(
        &oracle.compare(snapshot, &copy),
        "redis groups_data:made-up",
    );
    // a missing field and an extra field
    let RedisValueRepr::Hash(fields) = &hash.value else {
        unreachable!("the entry is a hash");
    };
    let field = fields.keys().next().unwrap().clone();
    let mut copy = snapshot.clone();
    if let RedisValueRepr::Hash(fields) = &mut copy.redis.get_mut(hash_key).unwrap().value {
        fields.remove(&field);
    }
    assert_only(
        &oracle.compare(snapshot, &copy),
        &format!("redis {hash_key} field {field}"),
    );
    let mut copy = snapshot.clone();
    if let RedisValueRepr::Hash(fields) = &mut copy.redis.get_mut(hash_key).unwrap().value {
        fields.insert("made_up".to_owned(), "1".to_owned());
    }
    assert_only(
        &oracle.compare(snapshot, &copy),
        &format!("redis {hash_key} field made_up"),
    );
    // an extra set member
    let mut copy = snapshot.clone();
    if let RedisValueRepr::Set(members) = &mut copy.redis.get_mut(set_key).unwrap().value {
        members.insert("made-up".to_owned());
    }
    assert_only(
        &oracle.compare(snapshot, &copy),
        &format!("redis {set_key} member made-up"),
    );
}

/// A change made to one object's recorded properties
type Change = fn(&mut ObjectRepr);

/// Changing any recorded property of an object, or removing or adding an object, is reported there
#[tokio::test]
async fn every_object_change_is_found() {
    // each recorded property of one object of every bucket that has one
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    let mut roles = std::collections::BTreeSet::new();
    for ((role, name), object) in &snapshot.s3 {
        if !roles.insert(role.clone()) {
            continue;
        }
        let changes: [(&str, Change); 4] = [
            ("len", |object| object.len += 1),
            ("sha256", |object| object.sha256.push('0')),
            ("content_type", |object| {
                object.content_type = Some("text/changed".to_owned());
            }),
            ("metadata", |object| {
                object.metadata.insert("changed".to_owned(), "1".to_owned());
            }),
        ];
        for (property, change) in changes {
            let mut copy = snapshot.clone();
            change(copy.s3.get_mut(&(role.clone(), name.clone())).unwrap());
            assert_only(
                &oracle.compare(snapshot, &copy),
                &format!("s3 {role}/{name} {property}"),
            );
        }
        // a missing object and an extra one
        let mut copy = snapshot.clone();
        copy.s3.remove(&(role.clone(), name.clone()));
        assert_only(
            &oracle.compare(snapshot, &copy),
            &format!("s3 {role}/{name}"),
        );
        let mut copy = snapshot.clone();
        copy.s3
            .insert((role.clone(), "made-up".to_owned()), object.clone());
        assert_only(
            &oracle.compare(snapshot, &copy),
            &format!("s3 {role}/made-up"),
        );
    }
    for role in ["files", "repos", "graphics"] {
        assert!(roles.contains(role), "the fixture holds no {role} object");
    }
}

/// Changing any census stream is reported at that stream
#[tokio::test]
async fn every_census_change_is_found() {
    // drop the last bucket of each stream in turn
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    assert!(!snapshot.census.is_empty(), "the fixture's census is empty");
    for key in snapshot.census.keys() {
        let mut copy = snapshot.clone();
        copy.census.get_mut(key).unwrap().push("made-up".to_owned());
        assert_only(&oracle.compare(snapshot, &copy), &format!("census {key}"));
    }
}

/// Every difference is listed, not only the first
#[tokio::test]
async fn every_difference_is_listed() {
    // change two rows of two tables and one object
    let snapshot = seeded().await;
    let oracle = unchanged_oracle();
    let mut copy = snapshot.clone();
    let mut changed_tables = 0;
    for rows in copy.scylla.values_mut() {
        if let Some(key) = rows.keys().next().cloned() {
            rows.remove(&key);
            changed_tables += 1;
        }
        if changed_tables == 2 {
            break;
        }
    }
    let object = copy.s3.keys().next().unwrap().clone();
    copy.s3.remove(&object);
    assert_eq!(oracle.compare(snapshot, &copy).len(), 3);
}

/// A recomputed column that doesn't match its source is reported
#[tokio::test]
async fn a_wrong_recomputed_value_is_found() {
    // the oracle a restore is checked with
    let snapshot = seeded().await;
    let oracle = Oracle::default();
    // a recomputed column that doesn't match its source fails
    let (key, row) = snapshot.scylla["tags"].iter().next().unwrap();
    let mut copy = snapshot.clone();
    copy.scylla
        .get_mut("tags")
        .unwrap()
        .get_mut(key)
        .unwrap()
        .insert("key_lower".to_owned(), changed(&row["key_lower"]));
    let differences = oracle.compare(snapshot, &copy);
    assert!(
        differences
            .iter()
            .any(|difference| difference.location == format!("tags [{key}] key_lower")),
        "a wrong recomputed value passed"
    );
}

/// A TTL above what was left at backup is reported and one within it isn't
#[tokio::test]
async fn a_ttl_above_its_bound_is_found() {
    // the oracle a restore of a format that keeps TTLs is checked with
    let snapshot = seeded().await;
    let oracle = Oracle {
        format: Some(2),
        ..Oracle::default()
    };
    // a TTL above what was left at backup fails, one within it passes
    let (table, ttls) = snapshot
        .ttls
        .iter()
        .next()
        .expect("the fixture has rows with TTLs");
    let (row_key, left) = ttls
        .iter()
        .find(|(_, ttl)| ttl.is_some())
        .expect("the fixture has an expiring row");
    let left = left.unwrap();
    let mut above = snapshot.clone();
    above
        .ttls
        .get_mut(table)
        .unwrap()
        .insert(row_key.clone(), Some(left + 1000));
    assert!(
        oracle
            .compare(snapshot, &above)
            .iter()
            .any(|difference| difference.location.ends_with("ttl()"))
    );
    let mut within = snapshot.clone();
    within
        .ttls
        .get_mut(table)
        .unwrap()
        .insert(row_key.clone(), Some(left - 1));
    assert!(
        !oracle
            .compare(snapshot, &within)
            .iter()
            .any(|difference| difference.location.ends_with("ttl()"))
    );
}

/// An EPHEMERAL key left after a restore is reported
#[tokio::test]
async fn an_ephemeral_key_left_after_a_restore_is_found() {
    // the oracle a restore is checked with
    let snapshot = seeded().await;
    let oracle = Oracle::default();
    // an EPHEMERAL key left after a restore fails
    let mut ephemeral = snapshot.clone();
    ephemeral.redis.insert(
        "reaction_data:made-up".to_owned(),
        RedisEntry {
            value: RedisValueRepr::String("1".to_owned()),
            expires: false,
        },
    );
    assert!(
        is_ephemeral("reaction_data:made-up"),
        "{EPHEMERAL_FAMILIES} doesn't cover reaction data"
    );
    assert!(
        oracle
            .compare(snapshot, &ephemeral)
            .iter()
            .any(|difference| difference.location == "redis reaction_data:made-up")
    );
}

/// A node health a restore didn't reset is reported and a reset one isn't
#[tokio::test]
async fn a_node_health_that_was_not_reset_is_found() {
    // the oracle a restore is checked with
    let snapshot = seeded().await;
    let oracle = Oracle::default();
    // a node whose health wasn't reset fails, a reset one passes
    let (node, row) = snapshot.scylla["nodes"]
        .iter()
        .find(|(_, row)| row.contains_key("health"))
        .unwrap();
    let health = |copy: &Snapshot| {
        oracle
            .compare(snapshot, copy)
            .iter()
            .any(|difference| difference.location == format!("nodes [{node}] health"))
    };
    let mut kept = snapshot.clone();
    kept.scylla
        .get_mut("nodes")
        .unwrap()
        .get_mut(node)
        .unwrap()
        .insert("health".to_owned(), row["health"].clone());
    let mut reset = snapshot.clone();
    reset
        .scylla
        .get_mut("nodes")
        .unwrap()
        .get_mut(node)
        .unwrap()
        .insert(
            "health".to_owned(),
            "Text(\"\\\"Registered\\\"\")".to_owned(),
        );
    assert!(
        row["health"].contains("Registered") || health(&kept),
        "a node health that wasn't reset passed"
    );
    assert!(!health(&reset), "a reset node health failed");
}

/// A column format 1 doesn't carry that a restore filled in is reported
#[tokio::test]
async fn a_filled_format_two_column_is_found() {
    // the oracle a restore of a format-1 backup is checked with
    let snapshot = seeded().await;
    let oracle = Oracle {
        format: Some(1),
        ..Oracle::default()
    };
    // a format-2 column a format-1 restore filled in fails
    let (key, row) = snapshot.scylla["results"]
        .iter()
        .find(|(_, row)| row.contains_key("tool_version"))
        .unwrap();
    let mut filled = snapshot.clone();
    filled
        .scylla
        .get_mut("results")
        .unwrap()
        .get_mut(key)
        .unwrap()
        .insert("tool_version".to_owned(), row["tool_version"].clone());
    assert!(
        oracle
            .compare(snapshot, &filled)
            .iter()
            .any(|difference| difference.location == format!("results [{key}] tool_version"))
    );
}

/// Every rule names a location that exists, and a rule naming one that doesn't is reported
#[tokio::test]
async fn rules_name_only_existing_locations() {
    // read the live columns of every table the rules may name
    let harness = Harness::get().await;
    let scylla = harness.scylla().await;
    let mut tables = Vec::new();
    for name in snapshot_tables() {
        tables.push(live_table(&scylla, &harness.conf.thorium.namespace, name).await);
    }
    assert_eq!(stale_rules(ORACLE_RULES, &tables), Vec::<String>::new());
    let stale = [OracleRule {
        location: "samples_list.no_such_column",
        kind: OracleRuleKind::Ignore,
        reason: "made up",
    }];
    assert_eq!(
        stale_rules(&stale, &tables),
        vec!["ORACLE_RULES names samples_list.no_such_column, which doesn't exist".to_owned()]
    );
}

/// For format 1, which keeps no TTLs, a restored row with its table's default TTL passes, a
/// permanent row given the default passes, and a TTL above the default or a missing TTL on a table
/// with a default is found
#[tokio::test]
async fn format_one_ttls_are_the_table_default() {
    // the oracle a restore of a format-1 backup is checked with
    let snapshot = seeded().await;
    let oracle = Oracle {
        format: Some(1),
        ..Oracle::default()
    };
    let (table, ttls) = snapshot
        .ttls
        .iter()
        .next()
        .expect("the fixture has rows with TTLs");
    let default = snapshot.default_ttls[table];
    assert!(default > 0, "{table} has no default TTL");
    let with = |ttl: Option<i32>| {
        let mut changed = snapshot.clone();
        for row in changed.ttls.get_mut(table).unwrap().values_mut() {
            *row = ttl;
        }
        oracle
            .compare(snapshot, &changed)
            .into_iter()
            .filter(|difference| difference.location.ends_with("ttl()"))
            .count()
    };
    // every row at or under the default passes, whatever TTL it had
    assert!(ttls.values().any(Option::is_some));
    assert_eq!(with(Some(default)), 0);
    assert_eq!(with(Some(default - 30)), 0);
    // a TTL above the default, or none on a table with a default, is found for every row
    assert_eq!(with(Some(default + 1)), ttls.len());
    assert_eq!(with(None), ttls.len());
}

/// A Redis key a snapshot can't read is kept, and differs even from itself, instead of being
/// left out of both sides of a comparison
#[tokio::test]
async fn an_unreadable_key_is_always_a_difference() {
    // add a stream, a type no snapshot reads, once the seeded snapshot is taken
    let _ = seeded().await;
    let live = LIVE.lock().await;
    let harness = Harness::get().await;
    let key = format!("{}:unreadable_probe", harness.conf.thorium.namespace);
    let pool = common::harness::redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let _: String = redis::cmd("XADD")
        .arg(&key)
        .arg("*")
        .arg("field")
        .arg("value")
        .query_async(&mut *conn)
        .await
        .unwrap();
    let snapshot = Snapshot::take(harness).await;
    let _: () = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .unwrap();
    drop(live);
    // the key is in the snapshot as unreadable, and comparing the snapshot with itself shows it
    let entry = snapshot.redis.get("unreadable_probe");
    assert!(
        matches!(
            entry.map(|entry| &entry.value),
            Some(RedisValueRepr::Unreadable(_))
        ),
        "{entry:?}"
    );
    let differences: Vec<Difference> = Oracle::default()
        .compare(&snapshot, &snapshot)
        .into_iter()
        .filter(|difference| difference.location == "redis unreadable_probe")
        .collect();
    assert_eq!(differences.len(), 1, "{differences:?}");
}
