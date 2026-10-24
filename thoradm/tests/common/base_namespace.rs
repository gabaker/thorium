//! The namespace the schema and migrate tests build their keyspaces from
//!
//! Each test names its keyspace `<base>_<suffix>`. The base comes from
//! `THORIUM__THORIUM__NAMESPACE`; without it a local run makes up a base of its own so two runs
//! never share keyspaces, and a CI run fails because the pipeline must name its job's namespace.
//! Each test binary that includes this file uses only the parts it needs.
#![allow(dead_code)]

use std::sync::OnceLock;

/// The environment variable that names the base namespace
pub const NAMESPACE_VAR: &str = "THORIUM__THORIUM__NAMESPACE";

/// The prefix every base namespace must start with, so a test never touches a real namespace
pub const TEST_PREFIX: &str = "testing_";

/// The base namespace of this process, chosen once
static BASE: OnceLock<String> = OnceLock::new();

/// Decide the base namespace
///
/// # Arguments
///
/// * `configured` - The value of `THORIUM__THORIUM__NAMESPACE`, if it is set
/// * `ci` - Whether the `CI` environment variable is `true`
/// * `make` - Makes up a namespace for a local run that names none
pub fn choose(
    configured: Option<&str>,
    ci: bool,
    make: impl FnOnce() -> String,
) -> Result<String, String> {
    // a namespace the environment names must be a test namespace
    match configured.filter(|name| !name.is_empty()) {
        Some(name) if name.starts_with(TEST_PREFIX) => Ok(name.to_lowercase()),
        Some(name) => Err(format!(
            "refusing {NAMESPACE_VAR}={name:?}: a test namespace must start with {TEST_PREFIX:?}"
        )),
        // CI must name its job's namespace, and a local run makes one up
        None if ci => Err(format!(
            "set {NAMESPACE_VAR} to this job's namespace (testing_ci_<stamp>_<random>): CI runs \
             never share a default namespace"
        )),
        None => Ok(make()),
    }
}

/// Build a namespace unique to one process: `testing_ci_<stamp>_<random>`
///
/// # Arguments
///
/// * `epoch_minutes` - The current time in minutes since the Unix epoch
/// * `random` - Random bits that tell concurrent processes apart
#[must_use]
pub fn generated(epoch_minutes: u64, random: u32) -> String {
    // the stamp in seven hex digits, then the random part in eight
    format!("{TEST_PREFIX}ci_{epoch_minutes:07x}_{random:08x}")
}

/// Returns this process's base namespace, choosing it on first use
///
/// # Panics
///
/// Panics when the environment gives no usable namespace.
#[must_use]
pub fn base_namespace() -> String {
    // choose once so every test of the binary agrees
    BASE.get_or_init(|| {
        let configured = std::env::var(NAMESPACE_VAR).ok();
        let ci = std::env::var("CI").is_ok_and(|value| value == "true");
        choose(configured.as_deref(), ci, || {
            // the stamp is the current epoch minute and the random part tells processes apart
            let minutes = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs() / 60);
            generated(minutes, rand::random())
        })
        .unwrap_or_else(|err| panic!("{err}"))
    })
    .clone()
}
