//! Checks the Harness tool spec the `thorpal-harness` page tells users to register, and the one
//! the `ThorPal` toolbox (`thorpal/toolbox`) imports

use std::process::Command;
use thorium::models::{HarnessCapabilities, HarnessSpec, OutputDisplayType, ToolSpec};

/// The Tool Developers page that walks through registering `thorpal-harness`
const PAGE: &str = include_str!("../../api/docs/src/developers/thorpal_harness.md");

/// The Harness tool spec the `ThorPal` toolbox imports
const TOOLBOX_SPEC: &str = include_str!("../toolbox/harness/thorpal-harness.json");

/// Get the Harness tool spec shown on the `thorpal-harness` page
fn documented_spec() -> ToolSpec {
    // the spec is the yaml block that starts with its file name
    let start = PAGE
        .find("# thorpal.yaml\n")
        .expect("the page shows thorpal.yaml");
    // the block ends at the next code fence
    let len = PAGE[start..]
        .find("```")
        .expect("the thorpal.yaml block is closed");
    // read the yaml document, whose top-level key names the spec's kind
    let value: serde_json::Value =
        serde_norway::from_str(&PAGE[start..start + len]).expect("thorpal.yaml is valid yaml");
    // parse the document the way thorctl reads a spec file keyed by its kind
    serde_json::from_value(value).expect("thorpal.yaml is a valid tool spec")
}

/// `thorpal-harness` writes its answer as plain text, so its registered spec stores the answer
/// as a string result rather than JSON
#[test]
fn documented_spec_stores_the_answer_as_text() {
    // the documented spec is a Harness tool
    let ToolSpec::Harness(harness) = documented_spec() else {
        panic!("thorpal.yaml is not a Harness tool");
    };
    // the answer's display type comes from the harness workload's data settings
    assert_eq!(
        harness.workload.data.display_type,
        OutputDisplayType::String
    );
}

/// Get the Harness tool spec the `ThorPal` toolbox imports
fn toolbox_spec() -> HarnessSpec {
    // parse the spec file the way thorctl reads a spec keyed by its kind
    let spec: ToolSpec =
        serde_json::from_str(TOOLBOX_SPEC).expect("thorpal-harness.json is a valid tool spec");
    // the toolbox's harness tool is a Harness tool
    let ToolSpec::Harness(harness) = spec else {
        panic!("thorpal-harness.json is not a Harness tool");
    };
    harness
}

/// Get a Harness spec as JSON with its container image left out
///
/// # Arguments
///
/// * `spec` - The spec to convert
fn without_image(spec: &HarnessSpec) -> serde_json::Value {
    // convert the spec to JSON
    let mut value = serde_json::to_value(spec).expect("a Harness spec serializes");
    // the page names a placeholder image and the toolbox derives its image at build time
    if let Some(image) = value.pointer_mut("/workload/environment/K8s/image") {
        *image = serde_json::Value::Null;
    }
    value
}

/// The toolbox registers the harness exactly as the `thorpal-harness` page does, apart from the
/// image, so importing the toolbox and following the page give the same Harness tool
#[test]
fn toolbox_spec_matches_the_documented_spec() {
    // the documented spec is a Harness tool
    let ToolSpec::Harness(documented) = documented_spec() else {
        panic!("thorpal.yaml is not a Harness tool");
    };
    // both specs agree on everything but the image
    assert_eq!(without_image(&toolbox_spec()), without_image(&documented));
}

/// The toolbox's Harness tool declares the capabilities `thorpal-harness --capabilities` prints
#[test]
fn toolbox_spec_declares_the_binary_capabilities() {
    // ask the harness binary for its capabilities
    let output = Command::new(env!("CARGO_BIN_EXE_thorpal-harness"))
        .arg("--capabilities")
        .output()
        .expect("thorpal-harness runs");
    assert!(
        output.status.success(),
        "thorpal-harness --capabilities failed"
    );
    // the binary prints them as JSON
    let printed: HarnessCapabilities =
        serde_json::from_slice(&output.stdout).expect("the capabilities are JSON");
    // the toolbox spec declares the same capabilities
    assert_eq!(toolbox_spec().capabilities, printed);
}
