//! The users example's fixtures: users in layout 1 and layout 2, as hashes the harness seeds and
//! as complete users written into a test deployment's Redis
#![allow(dead_code)]

use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use thorium::models::upgrades::{Record, plan_fixtures::USERS};
use thorium::test_utilities::upgrade_harness::{FamilyFixture, Layout, Variation};

/// The AI settings of a user who has any
pub const AI: &str = r#"{"endpoints":{},"default_endpoint":"x"}"#;

/// The themes the fixtures cycle through
const THEMES: [&str; 5] = ["Dark", "Light", "Crab", "Ocean", "Automatic"];

/// The username of the nth fixture user
///
/// # Arguments
///
/// * `prefix` - What makes the users of one test unique
/// * `index` - The user's number
#[must_use]
pub fn username(prefix: &str, index: usize) -> String {
    // zero padded so the names sort in the order they were made
    format!("{prefix}{index:03}")
}

/// The fields of a user in layout 1: the theme in its own field, and `settings` only for a user
/// with AI settings
///
/// # Arguments
///
/// * `name` - The username
/// * `index` - The user's number, which picks the theme and whether it has AI settings
#[must_use]
pub fn layout_1_fields(name: &str, index: usize) -> Vec<(String, String)> {
    // the fields every user has
    let mut fields = base_fields(name);
    fields.push((
        "theme".to_owned(),
        format!("\"{}\"", THEMES[index % THEMES.len()]),
    ));
    // only a user with AI settings has a settings field in layout 1
    if index % 3 == 0 {
        fields.push(("settings".to_owned(), format!(r#"{{"ai":{AI}}}"#)));
    }
    fields
}

/// The fields of a user in layout 2: the theme inside `settings`
///
/// # Arguments
///
/// * `name` - The username
/// * `index` - The user's number, which picks the theme and whether it has AI settings
#[must_use]
pub fn layout_2_fields(name: &str, index: usize) -> Vec<(String, String)> {
    // the fields every user has
    let mut fields = base_fields(name);
    let mut settings = serde_json::json!({ "theme": THEMES[index % THEMES.len()] });
    if index % 3 == 0 {
        settings["ai"] = serde_json::from_str(AI).expect("ai json");
    }
    fields.push(("settings".to_owned(), settings.to_string()));
    fields
}

/// The fields every user has, whatever its layout
///
/// # Arguments
///
/// * `name` - The username
fn base_fields(name: &str) -> Vec<(String, String)> {
    // an ordinary user with a token that never expires
    vec![
        ("username".to_owned(), name.to_owned()),
        ("email".to_owned(), format!("{name}@example.com")),
        ("role".to_owned(), "\"User\"".to_owned()),
        ("token".to_owned(), format!("token-{name}")),
        (
            "token_expiration".to_owned(),
            "\"2099-01-01T00:00:00Z\"".to_owned(),
        ),
        ("verified".to_owned(), "true".to_owned()),
        ("aliases".to_owned(), "{}".to_owned()),
    ]
}

/// Write `count` users in layout 1 into a deployment's Redis, replacing any of the same names
///
/// This is the integration test step that keeps the example's end-to-end path reproducible once
/// the only code that writes layout 1 is gone.
///
/// # Arguments
///
/// * `pool` - The Redis pool of the deployment
/// * `namespace` - The deployment's namespace
/// * `prefix` - What makes the users of one test unique
/// * `count` - How many users to write
pub async fn seed_layout_1(
    pool: &Pool<RedisConnectionManager>,
    namespace: &str,
    prefix: &str,
    count: usize,
) -> Vec<String> {
    // write each user's hash and add it to the user index
    let mut conn = pool.get().await.expect("a Redis connection");
    let mut names = Vec::new();
    for index in 0..count {
        let name = username(prefix, index);
        let key = format!("{namespace}:user_data:{name}");
        let mut pipe = redis::pipe();
        pipe.cmd("DEL").arg(&key).ignore();
        for (field, value) in layout_1_fields(&name, index) {
            pipe.cmd("HSET").arg(&key).arg(field).arg(value).ignore();
        }
        pipe.cmd("SADD")
            .arg(format!("{namespace}:users"))
            .arg(&name)
            .ignore();
        pipe.query_async::<()>(&mut *conn).await.expect("seeded");
        names.push(name);
    }
    names
}

/// Remove users a test wrote
///
/// # Arguments
///
/// * `pool` - The Redis pool of the deployment
/// * `namespace` - The deployment's namespace
/// * `names` - The usernames to remove
pub async fn remove(pool: &Pool<RedisConnectionManager>, namespace: &str, names: &[String]) {
    // delete each hash and drop the name from the index
    let mut conn = pool.get().await.expect("a Redis connection");
    for name in names {
        let mut pipe = redis::pipe();
        pipe.cmd("DEL")
            .arg(format!("{namespace}:user_data:{name}"))
            .ignore();
        pipe.cmd("SREM")
            .arg(format!("{namespace}:users"))
            .arg(name)
            .ignore();
        pipe.query_async::<()>(&mut *conn).await.expect("removed");
    }
}

/// Read a user's hash as a record
///
/// # Arguments
///
/// * `pool` - The Redis pool of the deployment
/// * `namespace` - The deployment's namespace
/// * `name` - The username
pub async fn read(pool: &Pool<RedisConnectionManager>, namespace: &str, name: &str) -> Record {
    // every field of the hash, keyed relative to the namespace as the runner reads it
    let mut conn = pool.get().await.expect("a Redis connection");
    let fields = redis::cmd("HGETALL")
        .arg(format!("{namespace}:user_data:{name}"))
        .query_async(&mut *conn)
        .await
        .expect("the hash reads");
    Record::Redis {
        key: format!("user_data:{name}"),
        fields,
    }
}

/// Build a user for the harness: the family is keyed `users:<name>` there
///
/// # Arguments
///
/// * `layout` - The layout, 1 or 2
/// * `index` - The user's number
/// * `_variation` - Unused: users declare no variants here
fn generate(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // the same fields a deployment holds, in the requested layout
    let name = username("example", index);
    let fields = if layout.0 == 1 {
        layout_1_fields(&name, index)
    } else {
        layout_2_fields(&name, index)
    };
    Record::Redis {
        key: format!("users:{name}"),
        fields: fields.into_iter().collect(),
    }
}

/// Find the layout a user is in from the fields it holds
///
/// # Arguments
///
/// * `record` - The user
fn layout_of(record: &Record) -> Option<Layout> {
    // a flat theme is layout 1, and a theme inside settings is layout 2
    if record.has_field("theme") {
        return Some(Layout(1));
    }
    let settings = record.text("settings")?;
    let settings: serde_json::Value = serde_json::from_str(&settings).ok()?;
    settings.get("theme").map(|_| Layout(2))
}

/// How the harness builds and reads the users family
#[must_use]
pub fn fixture() -> FamilyFixture {
    // no corruption override
    FamilyFixture {
        generate,
        layout_of,
        corrupt: None,
    }
}

/// The family the example rewrites
pub const FAMILY: thorium::models::upgrades::Family = USERS;
