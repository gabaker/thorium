//! Tests for toolbox v2 types, loading, validation, listing, and plan types

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use thorium::Error;
use thorium::models::{K8sEnvironment, ToolSpec, WorkloadEnvironment, WorkloadSpec};
use uuid::Uuid;

use super::load::{Loaded, parse_any};
use super::validate::{Issue, validate_manifest};
use crate::handlers::progress::{Bar, BarKind};

mod list;
mod load;
mod plan;
mod refs;
mod types;
mod validate;

/// The repository's toolbox in the legacy format, kept from before its manifests became tools
pub(super) const REAL_TOOLBOX: &str = include_str!("../../../../../../tools/toolbox.v1.json");

/// A small valid v2 toolbox holding one of each kind of item
pub(super) const CLEAN_V2: &str =
    include_str!("../../../../../tests/data/toolbox_v2/clean.v2.json");

/// A small v2 toolbox with several mistakes
pub(super) const BROKEN_V2: &str =
    include_str!("../../../../../tests/data/toolbox_v2/broken.v2.json");

/// A small legacy toolbox with an image that can't convert and a pipeline that depends on it
pub(super) const SMALL_LEGACY: &str =
    include_str!("../../../../../tests/data/toolbox_v2/small.legacy.json");

/// The directory the golden files live in
pub(super) fn golden_dir() -> PathBuf {
    // the goldens sit with the other test data of this crate
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/toolbox_v2")
}

/// Compare text with a golden file, or rewrite the file when `THORCTL_UPDATE_GOLDEN` is set
///
/// # Arguments
///
/// * `name` - The golden file's name
/// * `actual` - The text to compare
pub(super) fn assert_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    // an update run records the output instead of checking it
    if std::env::var_os("THORCTL_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    // otherwise the output must match the recorded file exactly
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read golden {}: {err}", path.display()));
    assert_eq!(actual, expected, "golden {name} differs");
}

/// Build a progress bar that draws nothing
pub(super) fn quiet() -> Bar {
    // a quiet bar has nothing to draw, so tests stay silent
    Bar::new_or_quiet("test", "test", BarKind::UnboundIO, true)
}

/// Load a manifest document
///
/// # Arguments
///
/// * `document` - The manifest's JSON text
pub(super) async fn load_text(document: &str) -> Result<Loaded, Error> {
    // parse with a quiet bar and no base directory
    parse_any(document.as_bytes(), None, &quiet()).await
}

/// Load a manifest document built from a JSON value
///
/// # Arguments
///
/// * `document` - The manifest as a JSON value
pub(super) async fn load(document: &Value) -> Result<Loaded, Error> {
    // serialize the value and load it
    load_text(&document.to_string()).await
}

/// Get the codes of a list of issues
///
/// # Arguments
///
/// * `issues` - The issues to read the codes of
pub(super) fn codes(issues: &[Issue]) -> BTreeSet<String> {
    // collect the distinct codes
    issues.iter().map(|issue| issue.code.clone()).collect()
}

/// Validate a loaded manifest with no unreadable sources
///
/// # Arguments
///
/// * `loaded` - The loaded manifest
pub(super) fn issues_of(loaded: &Loaded) -> Vec<Issue> {
    // check the manifest with the report loading produced
    validate_manifest(&loaded.manifest, &loaded.report, &[])
}

/// Build the JSON of an Image tool spec
///
/// # Arguments
///
/// * `image` - The container image the spec runs
pub(super) fn image_spec(image: &str) -> Value {
    // build a typed spec so the JSON is whatever the API model serializes
    let spec = ToolSpec::Image(WorkloadSpec {
        environment: WorkloadEnvironment::K8s(K8sEnvironment {
            image: image.to_owned(),
            ..K8sEnvironment::default()
        }),
        ..WorkloadSpec::default()
    });
    serde_json::to_value(spec).expect("serialize spec")
}

/// Build an Image tool spec whose dependencies hold references
///
/// # Arguments
///
/// * `results` - The values of `data.dependencies.results.tools`
pub(super) fn image_spec_depending_on(results: &[&str]) -> Value {
    // start from a plain spec and fill in the slot
    let mut spec = image_spec("example/tool:1");
    spec["Image"]["data"]["dependencies"]["results"]["tools"] = json!(results);
    spec
}

/// Build the JSON of a manifest with one group's worth of tools
///
/// # Arguments
///
/// * `tools` - The `tools` section
pub(super) fn with_tools(tools: &Value) -> Value {
    // a named manifest holding only tools
    json!({"name": "test", "tools": tools})
}

/// Build a one version Image tool
///
/// # Arguments
///
/// * `spec` - The version's spec
pub(super) fn tool_of(spec: &Value) -> Value {
    // a tool in group `g` with a single version `1`
    json!({"groups": ["g"], "versions": {"1": {"spec": spec}}})
}

/// A scratch directory that is removed when it goes out of scope
pub(super) struct Scratch(pub(super) PathBuf);

impl Scratch {
    /// Create an empty directory under the system temp directory
    pub(super) fn new() -> Self {
        // a unique name keeps parallel tests apart
        let path = std::env::temp_dir().join(format!("thorctl-v2-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Scratch(path)
    }
}

impl Drop for Scratch {
    /// Remove the directory and everything in it
    fn drop(&mut self) {
        // best effort: a leftover temp directory is harmless
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
