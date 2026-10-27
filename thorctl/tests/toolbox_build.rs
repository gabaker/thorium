//! Tests that run `thorctl toolbox build`, `init tool`, and `build-images` as programs
//!
//! None of these commands talk to Thorium, so no test starts the test API, logs in, or has a
//! config file; each runs with an empty home directory and a config path that does not exist.
//! `build-images` runs against a fake `docker` script on the `PATH` that records its arguments.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A scratch directory removed when dropped
struct Scratch(PathBuf);

impl Scratch {
    /// Create an empty scratch directory with a unique name
    fn new() -> Self {
        // a random name keeps parallel tests apart
        let path = std::env::temp_dir().join(format!("thorctl-build-bin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Scratch(path)
    }

    /// Get a path inside the scratch directory
    ///
    /// # Arguments
    ///
    /// * `relative` - The path relative to the scratch directory
    fn path(&self, relative: &str) -> PathBuf {
        // paths in these tests are always relative and inside the scratch directory
        self.0.join(relative)
    }

    /// Write a file inside the scratch directory, creating its directories
    ///
    /// # Arguments
    ///
    /// * `relative` - The path relative to the scratch directory
    /// * `contents` - The file's contents
    fn write(&self, relative: &str, contents: &str) {
        // make the directories the file lives in first
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dirs");
        std::fs::write(path, contents).expect("write file");
    }

    /// Read a file inside the scratch directory
    ///
    /// # Arguments
    ///
    /// * `relative` - The path relative to the scratch directory
    fn read(&self, relative: &str) -> String {
        // the files read here were written by the commands under test
        std::fs::read_to_string(self.path(relative)).expect("read file")
    }

    /// Read a JSON file inside the scratch directory
    ///
    /// # Arguments
    ///
    /// * `relative` - The path relative to the scratch directory
    fn json(&self, relative: &str) -> Value {
        // parse the file as JSON
        serde_json::from_str(&self.read(relative)).expect("valid json")
    }
}

impl Drop for Scratch {
    /// Remove the directory
    fn drop(&mut self) {
        // leftovers in the temp directory are harmless
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copy a directory tree
///
/// # Arguments
///
/// * `from` - The directory to copy
/// * `to` - Where to copy it to
fn copy_dir(from: &Path, to: &Path) {
    // make the target, then copy each entry, descending into directories
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("a dir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

/// List every file under a directory
///
/// # Arguments
///
/// * `dir` - The directory to list
fn walk(dir: &Path) -> Vec<PathBuf> {
    // descend into each directory and collect the files
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("a dir entry").path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// Run `thorctl toolbox` with no config and no network
///
/// # Arguments
///
/// * `home` - The empty home directory to run with
/// * `dir` - The directory to run in
/// * `args` - The arguments after `toolbox`
fn toolbox(home: &Path, dir: &Path, args: &[&str]) -> Output {
    // the config path does not exist, so any attempt to log in or connect would fail the test
    Command::new(env!("CARGO_BIN_EXE_thorctl"))
        .args([
            "--config",
            "/nonexistent/thorium.yml",
            "--skip-update",
            "toolbox",
        ])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env_remove("THORCTL__CLUSTERS")
        .output()
        .expect("run thorctl")
}

/// Get a command's stderr as text
///
/// # Arguments
///
/// * `output` - The finished command
fn stderr(output: &Output) -> String {
    // the text of stderr, lossily decoded
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Get a command's stdout as text
///
/// # Arguments
///
/// * `output` - The finished command
fn stdout(output: &Output) -> String {
    // the text of stdout, lossily decoded
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A YAML image spec with no container image yet
const IMAGE_SPEC_YAML: &str = "Image:
  environment:
    K8s:
      image: \"\"
      scheduling:
        resources:
          cpu: 1000
          memory: 1024
        spawn_limit: Unlimited
";

/// The same image spec as JSON
const IMAGE_SPEC_JSON: &str = r#"{"Image":{"environment":{"K8s":{"image":"","scheduling":{"resources":{"cpu":500,"memory":512},"spawn_limit":"Unlimited"}}}}}"#;

/// A toolbox config with a registry to derive image tags under
const CONFIG: &str = "name = \"Test Toolbox\"\nregistry = \"registry.test/org\"\n";

/// A tool manifest that builds an image from its own directory
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `spec_file` - The spec file the manifest points at
fn tool_manifest(name: &str, spec_file: &str) -> String {
    // a buildable image tool in the `static` group
    format!(
        "type = \"tool\"\nname = \"{name}\"\nversion = \"latest\"\nkind = \"image\"\n\
         groups = [\"static\"]\ndescription = \"Scans things\"\nspec_from = \"{spec_file}\"\n\
         build_path = \"./\"\nlatest = true\n"
    )
}

/// Lay out a toolbox directory with two tools, one with a YAML spec and one with a JSON spec
fn tool_toolbox() -> Scratch {
    // the config, then one directory per tool
    let scratch = Scratch::new();
    scratch.write("config.toml", CONFIG);
    scratch.write(
        "tools/scan/manifest.toml",
        &tool_manifest("scan", "scan.spec.yaml"),
    );
    scratch.write("tools/scan/scan.spec.yaml", IMAGE_SPEC_YAML);
    scratch.write("tools/scan/Dockerfile", "FROM scratch\n");
    scratch.write(
        "tools/other/manifest.toml",
        &tool_manifest("other", "other.spec.json"),
    );
    scratch.write("tools/other/other.spec.json", IMAGE_SPEC_JSON);
    scratch
}

/// A legacy image manifest, its config, and a build-only context in a toolbox directory
fn legacy_toolbox() -> Scratch {
    // one image with a config, one build-only base, and a pipeline that runs the image
    let scratch = Scratch::new();
    scratch.write("config.toml", CONFIG);
    scratch.write(
        "images/scan/manifest.toml",
        "name = \"scan\"\ntype = \"image\"\nconfig_from = \"scan.json\"\nbuild_path = \"./\"\n\
         version = \"latest\"\n",
    );
    scratch.write(
        "images/scan/scan.json",
        r#"{"group":"static","name":"scan","scaler":"K8s","image":"","resources":{"cpu":500,"memory":512}}"#,
    );
    scratch.write(
        "images/base/manifest.toml",
        "name = \"base\"\ntype = \"image\"\nbuild_path = \"./\"\nversion = \"1.0\"\n",
    );
    scratch.write(
        "pipelines/scan/manifest.toml",
        "name = \"scan\"\ntype = \"pipeline\"\nversion = \"latest\"\nconfig_from = \"scan.json\"\n\n\
         [images.scan]\nversion = \"latest\"\n",
    );
    scratch.write(
        "pipelines/scan/scan.json",
        r#"{"group":"static","name":"scan","order":[["scan"]],"sla":3600,"triggers":{}}"#,
    );
    scratch
}

/// Building tool manifests writes a v2 toolbox with each spec inline and each image's build
#[test]
fn build_reads_tool_manifests_with_yaml_and_json_specs() {
    let scratch = tool_toolbox();
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = scratch.json("toolbox.json");
    // the manifest is in the current format and holds both tools
    assert_eq!(manifest["format_version"], 2);
    assert_eq!(manifest["name"], "Test Toolbox");
    assert!(manifest.get("images").is_none());
    let scan = &manifest["tools"]["scan"];
    assert_eq!(scan["groups"][0], "static");
    assert_eq!(scan["kind"], "Image");
    assert_eq!(scan["latest"], "latest");
    // the spec is inline and names the derived image
    let version = &scan["versions"]["latest"];
    let image = &version["spec"]["Image"]["environment"]["K8s"]["image"];
    assert_eq!(image, "registry.test/org/scan:latest");
    assert_eq!(version["dir"], "tools/scan");
    assert_eq!(version["build"]["build_path"], "tools/scan");
    assert_eq!(version["build"]["build_image"], true);
    assert_eq!(
        version["build"]["image_tags"][0],
        "registry.test/org/scan:latest"
    );
    // the JSON spec was read too
    let other = &manifest["tools"]["other"]["versions"]["latest"];
    assert_eq!(
        other["spec"]["Image"]["environment"]["K8s"]["scheduling"]["resources"]["cpu"],
        500
    );
}

/// A tool manifest with no build keys carries no build section and keeps its spec's image
#[test]
fn tools_without_build_keys_keep_their_image() {
    let scratch = Scratch::new();
    scratch.write("config.toml", CONFIG);
    scratch.write(
        "tools/pre/manifest.toml",
        "type = \"tool\"\nname = \"pre\"\ngroups = [\"static\"]\nspec_from = \"pre.spec.yaml\"\n",
    );
    scratch.write(
        "tools/pre/pre.spec.yaml",
        &IMAGE_SPEC_YAML.replace("\"\"", "\"ghcr.io/other/pre:1\""),
    );
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let version = &scratch.json("toolbox.json")["tools"]["pre"]["versions"]["latest"];
    assert!(version.get("build").is_none(), "{version}");
    assert_eq!(
        version["spec"]["Image"]["environment"]["K8s"]["image"],
        "ghcr.io/other/pre:1"
    );
}

/// A tool built with `build_image = false` keeps the image its spec names
#[test]
fn build_image_false_pins_the_spec_image() {
    let scratch = Scratch::new();
    scratch.write("config.toml", CONFIG);
    scratch.write(
        "tools/pre/manifest.toml",
        "type = \"tool\"\nname = \"pre\"\ngroups = [\"static\"]\nspec_from = \"pre.spec.yaml\"\n\
         build_path = \"./\"\nbuild_image = false\n",
    );
    scratch.write(
        "tools/pre/pre.spec.yaml",
        &IMAGE_SPEC_YAML.replace("\"\"", "\"ghcr.io/other/pre:1\""),
    );
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let version = &scratch.json("toolbox.json")["tools"]["pre"]["versions"]["latest"];
    assert_eq!(version["build"]["build_image"], false);
    assert_eq!(
        version["spec"]["Image"]["environment"]["K8s"]["image"],
        "ghcr.io/other/pre:1"
    );
}

/// Legacy image manifests still build the legacy lexicon and hint at the conversion
#[test]
fn legacy_sources_write_the_legacy_lexicon() {
    let scratch = legacy_toolbox();
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = scratch.json("toolbox.json");
    assert!(manifest.get("format_version").is_none());
    assert!(manifest.get("tools").is_none());
    assert_eq!(
        manifest["images"]["scan"]["latest"]["config"]["image"],
        "registry.test/org/scan:latest"
    );
    assert_eq!(manifest["images"]["base"]["1.0"]["build_image"], true);
    assert_eq!(
        manifest["pipelines"]["scan"]["latest"]["images"]["scan"]["version"],
        "latest"
    );
    // the hint names the way to the current format
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("--format-version 2"), "{text}");
}

/// `--format-version 2` converts legacy sources into the same manifest `convert` writes
#[test]
fn format_version_two_converts_legacy_sources() {
    let scratch = legacy_toolbox();
    let built = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "build",
            "-c",
            "config.toml",
            "--format-version",
            "2",
            "-o",
            "v2/toolbox.json",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let manifest = scratch.json("v2/toolbox.json");
    assert_eq!(manifest["format_version"], 2);
    assert!(manifest["tools"]["scan"]["versions"]["latest"]["spec"]["Image"].is_object());
    // a build-only context becomes a `builds` entry
    assert_eq!(manifest["builds"]["base"]["1.0"]["build_image"], true);
    assert_eq!(
        manifest["pipelines"]["scan"]["versions"]["latest"]["stage_tools"]["scan"]["tool"],
        "scan"
    );
    // converting the legacy build gives the same bytes
    let legacy = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "-o", "v2/legacy.json"],
    );
    assert!(legacy.status.success(), "{}", stderr(&legacy));
    let converted = toolbox(
        &scratch.0,
        &scratch.0,
        &["convert", "v2/legacy.json", "-o", "v2/converted.json"],
    );
    assert!(converted.status.success(), "{}", stderr(&converted));
    assert_eq!(
        scratch.read("v2/toolbox.json"),
        scratch.read("v2/converted.json")
    );
}

/// `--format-version 1` refuses tool manifests and writes nothing
#[test]
fn format_version_one_refuses_tool_manifests() {
    let scratch = tool_toolbox();
    let output = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "--format-version", "1"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--format-version 2"),
        "{}",
        stderr(&output)
    );
    assert!(!scratch.path("toolbox.json").exists());
}

/// A directory with image and tool manifests builds one v2 toolbox, and a name in both fails
#[test]
fn mixed_directories_build_both_and_refuse_collisions() {
    let scratch = legacy_toolbox();
    scratch.write(
        "tools/scan2/manifest.toml",
        &tool_manifest("scan2", "scan2.spec.yaml"),
    );
    scratch.write("tools/scan2/scan2.spec.yaml", IMAGE_SPEC_YAML);
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = scratch.json("toolbox.json");
    assert!(manifest["tools"]["scan"].is_object());
    assert!(manifest["tools"]["scan2"].is_object());
    assert!(manifest["builds"]["base"].is_object());
    // a tool named like an image is a collision
    scratch.write(
        "tools/scan3/manifest.toml",
        &tool_manifest("scan", "scan3.spec.yaml"),
    );
    scratch.write("tools/scan3/scan3.spec.yaml", IMAGE_SPEC_YAML);
    let output = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "-o", "again.json"],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("scan"), "{}", stderr(&output));
    assert!(!scratch.path("again.json").exists());
}

/// A spec that does not parse fails the build before anything is written
#[test]
fn an_invalid_spec_fails_the_build() {
    let scratch = tool_toolbox();
    scratch.write("tools/scan/scan.spec.yaml", "Image:\n  colour: red\n");
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("scan"), "{}", stderr(&output));
    assert!(!scratch.path("toolbox.json").exists());
}

/// Toolsets, agents, workflows, and pipelines with stage tools build and validate
#[test]
fn entity_manifests_build_and_validate() {
    let scratch = tool_toolbox();
    scratch.write(
        "toolsets/kit/manifest.toml",
        "type = \"toolset\"\nname = \"kit\"\ngroups = [\"static\"]\nspec_from = \"kit.toolset.yaml\"\n",
    );
    scratch.write(
        "toolsets/kit/kit.toolset.yaml",
        "groups: [static]\ntools:\n  - tool: scan\n",
    );
    scratch.write(
        "agents/triage/manifest.toml",
        "type = \"agent\"\nname = \"triage\"\ngroups = [\"static\"]\nspec_from = \"triage.agent.yaml\"\n",
    );
    scratch.write(
        "agents/triage/triage.agent.yaml",
        "groups: [static]\ntools:\n  - tool: scan\ntoolsets:\n  - toolset: kit\n",
    );
    scratch.write(
        "workflows/flow/manifest.toml",
        "type = \"workflow\"\nname = \"flow\"\ngroups = [\"static\"]\nspec_from = \"flow.workflow.yaml\"\n",
    );
    scratch.write(
        "workflows/flow/flow.workflow.yaml",
        "groups: [static]\nagents: [triage]\nentry_agent: triage\n",
    );
    scratch.write(
        "pipelines/scan/manifest.toml",
        "type = \"pipeline\"\nname = \"scan\"\nversion = \"latest\"\nconfig_from = \"scan.json\"\n\n\
         [stage_tools.scan]\ntool = \"scan\"\nversion = \"latest\"\n",
    );
    scratch.write(
        "pipelines/scan/scan.json",
        r#"{"group":"static","name":"scan","order":[["scan"]],"sla":3600,"triggers":{}}"#,
    );
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = scratch.json("toolbox.json");
    assert_eq!(manifest["toolsets"]["kit"]["tools"][0]["tool"], "scan");
    assert_eq!(
        manifest["agents"]["triage"]["toolsets"][0]["toolset"],
        "kit"
    );
    assert_eq!(manifest["workflows"]["flow"]["entry_agent"], "triage");
    let stage = &manifest["pipelines"]["scan"]["versions"]["latest"]["stage_tools"]["scan"];
    assert_eq!(stage["version"], "latest");
    // the manifest passes validation
    let validated = toolbox(
        &scratch.0,
        &scratch.0,
        &["validate", "toolbox.json", "--strict"],
    );
    assert!(
        validated.status.success(),
        "{}{}",
        stdout(&validated),
        stderr(&validated)
    );
}

/// Several versions of one tool become one tool with a version each
#[test]
fn versions_of_one_tool_merge() {
    let scratch = tool_toolbox();
    scratch.write(
        "tools/scan/v2/manifest.toml",
        &tool_manifest("scan", "scan.spec.yaml")
            .replace("version = \"latest\"", "version = \"2.0.0\"")
            .replace("latest = true\n", ""),
    );
    scratch.write("tools/scan/v2/scan.spec.yaml", IMAGE_SPEC_YAML);
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let versions = &scratch.json("toolbox.json")["tools"]["scan"]["versions"];
    assert!(versions["latest"].is_object());
    assert_eq!(
        versions["2.0.0"]["build"]["image_tags"][0],
        "registry.test/org/scan:2.0.0"
    );
}

/// `--tag-suffix` is baked into the derived tags and the spec's image
#[test]
fn tag_suffix_applies_to_tool_tags() {
    let scratch = tool_toolbox();
    let output = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "--tag-suffix", "-br"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let version = &scratch.json("toolbox.json")["tools"]["scan"]["versions"]["latest"];
    assert_eq!(
        version["build"]["image_tags"][0],
        "registry.test/org/scan:latest-br"
    );
    assert_eq!(
        version["spec"]["Image"]["environment"]["K8s"]["image"],
        "registry.test/org/scan:latest-br"
    );
}

/// `--rewrite-manifests` converts a legacy directory in place, keeping `.bak` files, and the
/// rewritten directory builds the same toolbox
#[test]
fn rewrite_manifests_converts_a_directory_in_place() {
    let scratch = legacy_toolbox();
    let before = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "build",
            "-c",
            "config.toml",
            "--format-version",
            "2",
            "-o",
            "before.json",
        ],
    );
    assert!(before.status.success(), "{}", stderr(&before));
    let original = scratch.read("images/scan/manifest.toml");
    let rewrite = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "build",
            "-c",
            "config.toml",
            "--rewrite-manifests",
            "-o",
            "rewritten.json",
        ],
    );
    assert!(rewrite.status.success(), "{}", stderr(&rewrite));
    // the old manifests are kept beside the new ones
    assert_eq!(scratch.read("images/scan/manifest.toml.bak"), original);
    // a build-only context becomes a build manifest, with no spec to write
    assert!(scratch.path("images/base/manifest.toml.bak").exists());
    let base = scratch.read("images/base/manifest.toml");
    assert!(base.contains("type = \"build\""), "{base}");
    assert!(scratch.path("pipelines/scan/manifest.toml.bak").exists());
    // the image became a tool whose spec replaced its config, in the same file
    let manifest = scratch.read("images/scan/manifest.toml");
    assert!(manifest.contains("type = \"tool\""), "{manifest}");
    assert!(manifest.contains("spec_from = \"scan.json\""), "{manifest}");
    assert!(!manifest.contains("config_from"), "{manifest}");
    let spec = scratch.json("images/scan/scan.json");
    assert!(spec.get("Image").is_some(), "{spec}");
    assert!(scratch.read("images/scan/scan.json").ends_with("}\n"));
    assert_eq!(
        scratch.json("images/scan/scan.json.bak")["name"],
        "scan",
        "the old config is kept"
    );
    // nothing else was written beside the manifests
    assert!(!scratch.path("images/scan/scan.spec.yaml").exists());
    // the pipeline's images became stage tools
    let pipeline = scratch.read("pipelines/scan/manifest.toml");
    assert!(pipeline.contains("[stage_tools.scan]"), "{pipeline}");
    assert!(!pipeline.contains("[images."), "{pipeline}");
    // a rebuild of the rewritten directory writes the same manifest as the conversion did
    let after = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "-o", "after.json"],
    );
    assert!(after.status.success(), "{}", stderr(&after));
    assert_eq!(scratch.json("before.json"), scratch.json("after.json"));
    // a second rewrite finds nothing left to rewrite and keeps the first backups
    let again = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "build",
            "-c",
            "config.toml",
            "--rewrite-manifests",
            "-o",
            "again.json",
        ],
    );
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(scratch.read("images/scan/manifest.toml.bak"), original);
}

/// A rewritten directory whose legacy images reuse a rewritten image with `image_from` builds
/// again, to the same toolbox as the conversion did, bundled network policies included
#[test]
fn rewrite_manifests_keeps_image_from_children_buildable() {
    // the legacy toolbox with a bundled network policy on scan, plus an image that reuses the
    // scan image's container image
    let scratch = legacy_toolbox();
    scratch.write(
        "images/scan/manifest.toml",
        "name = \"scan\"\ntype = \"image\"\nconfig_from = \"scan.json\"\nbuild_path = \"./\"\n\
         network_policies_from = [\"open.policy.json\"]\nversion = \"latest\"\n",
    );
    scratch.write(
        "images/scan/open.policy.json",
        r#"{"name":"open","groups":["static"],"forced_policy":false,"default_policy":false}"#,
    );
    scratch.write(
        "images/scan-wide/manifest.toml",
        "name = \"scan-wide\"\ntype = \"image\"\nconfig_from = \"scan-wide.json\"\n\
         version = \"latest\"\n\n[image_from]\nname = \"scan\"\nversion = \"latest\"\n",
    );
    scratch.write(
        "images/scan-wide/scan-wide.json",
        r#"{"group":"static","name":"scan-wide","scaler":"K8s","image":"","resources":{"cpu":500,"memory":512}}"#,
    );
    let before = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "build",
            "-c",
            "config.toml",
            "--format-version",
            "2",
            "-o",
            "before.json",
        ],
    );
    assert!(before.status.success(), "{}", stderr(&before));
    // the rewrite turns scan into a tool and keeps the image_from child an image
    let rewrite = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "build",
            "-c",
            "config.toml",
            "--rewrite-manifests",
            "-o",
            "rewritten.json",
        ],
    );
    assert!(rewrite.status.success(), "{}", stderr(&rewrite));
    assert!(
        scratch
            .read("images/scan/manifest.toml")
            .contains("type = \"tool\"")
    );
    assert!(
        scratch
            .read("images/scan-wide/manifest.toml")
            .contains("[image_from]")
    );
    // the rewritten directory builds again, reusing the tool's image, to the same manifest
    let after = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "-o", "after.json"],
    );
    assert!(after.status.success(), "{}", stderr(&after));
    assert_eq!(scratch.json("before.json"), scratch.json("after.json"));
    assert_eq!(
        scratch.json("after.json")["network_policies"][0]["name"],
        "open"
    );
}

/// A base build manifest of the scratch toolbox
///
/// # Arguments
///
/// * `extra` - More top-level keys for the manifest
fn build_manifest(extra: &str) -> String {
    // a base image built from its own directory under a repo-style image path
    format!(
        "name = \"base\"\ntype = \"build\"\nversion = \"1.0\"\nbuild_path = \"./\"\n\
         image_name = \"tools/base\"\n{extra}"
    )
}

/// A `type = "build"` manifest writes a base build of the current format, with the same entry a
/// build-only legacy image converts to
#[test]
fn build_manifests_write_base_builds() {
    // the tool toolbox plus a base build manifest
    let scratch = tool_toolbox();
    scratch.write("bases/base/manifest.toml", &build_manifest(""));
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = scratch.json("toolbox.json");
    assert_eq!(manifest["format_version"], 2);
    let build = &manifest["builds"]["base"]["1.0"];
    assert_eq!(build["dir"], "bases/base", "{build}");
    assert_eq!(build["build_path"], "bases/base", "{build}");
    assert_eq!(build["build_image"], true, "{build}");
    assert_eq!(
        build["image_tags"],
        serde_json::json!(["registry.test/org/base:1.0"])
    );
    // the same base as a build-only legacy image converts to the very same entry
    scratch.write(
        "bases/base/manifest.toml",
        "name = \"base\"\ntype = \"image\"\nversion = \"1.0\"\nbuild_path = \"./\"\n\
         image_name = \"tools/base\"\n",
    );
    let output = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "-o", "legacy.json"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(scratch.json("legacy.json")["builds"], manifest["builds"]);
}

/// A build manifest alone writes the current format, and the legacy format refuses it
#[test]
fn build_manifests_need_the_current_format() {
    // a directory with only a base build
    let scratch = Scratch::new();
    scratch.write("config.toml", CONFIG);
    scratch.write("base/manifest.toml", &build_manifest("build = false\n"));
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = scratch.json("toolbox.json");
    assert_eq!(manifest["format_version"], 2);
    assert_eq!(manifest["builds"]["base"]["1.0"]["build_image"], false);
    let refused = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "--format-version", "1"],
    );
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("build"), "{}", stderr(&refused));
}

/// A build manifest that names a config, a spec, or an image to reuse is refused, since a base
/// build has none of them
#[test]
fn build_manifests_refuse_runtime_keys() {
    for extra in [
        "config_from = \"base.json\"\n",
        "spec_from = \"base.spec.yaml\"\n",
        "\n[image_from]\nname = \"other\"\n",
    ] {
        let scratch = Scratch::new();
        scratch.write("config.toml", CONFIG);
        scratch.write("base/manifest.toml", &build_manifest(extra));
        let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
        assert!(!output.status.success(), "{extra}");
        assert!(
            stderr(&output).contains("Build 'base@1.0'"),
            "{extra}: {}",
            stderr(&output)
        );
    }
}

/// An image that reuses a build-only base with `image_from` still builds after the rewrite turned
/// the base into a build manifest, to the same toolbox as before
#[test]
fn rewrite_manifests_keeps_image_from_children_of_a_base_buildable() {
    // the legacy toolbox plus an image that reuses the base's container image
    let scratch = legacy_toolbox();
    scratch.write(
        "images/base-run/manifest.toml",
        "name = \"base-run\"\ntype = \"image\"\nconfig_from = \"base-run.json\"\n\
         version = \"latest\"\n\n[image_from]\nname = \"base\"\nversion = \"1.0\"\n",
    );
    scratch.write(
        "images/base-run/base-run.json",
        r#"{"group":"static","name":"base-run","scaler":"K8s","image":"","resources":{"cpu":500,"memory":512}}"#,
    );
    let run = |args: &[&str]| {
        let output = toolbox(&scratch.0, &scratch.0, args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    };
    run(&[
        "build",
        "-c",
        "config.toml",
        "--format-version",
        "2",
        "-o",
        "before.json",
    ]);
    run(&[
        "build",
        "-c",
        "config.toml",
        "--rewrite-manifests",
        "-o",
        "rewritten.json",
    ]);
    assert!(
        scratch
            .read("images/base/manifest.toml")
            .contains("type = \"build\"")
    );
    run(&["build", "-c", "config.toml", "-o", "after.json"]);
    assert_eq!(scratch.json("before.json"), scratch.json("rewritten.json"));
    assert_eq!(scratch.json("before.json"), scratch.json("after.json"));
}

/// A tool manifest that reuses another item's image with `image_from`
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `from` - The name of the tool or build whose image it reuses
/// * `version` - The version of that tool or build
fn reusing_manifest(name: &str, from: &str, version: &str) -> String {
    // an image tool with a JSON spec and no build of its own
    format!(
        "type = \"tool\"\nname = \"{name}\"\nversion = \"latest\"\nkind = \"image\"\n\
         groups = [\"static\"]\nspec_from = \"{name}.json\"\n\n[image_from]\nname = \"{from}\"\n\
         version = \"{version}\"\n"
    )
}

/// A spec that runs the reused image with its own command and leaves the image empty
///
/// # Arguments
///
/// * `image` - The image the spec names
fn reusing_spec(image: &str) -> String {
    // the same K8s scheduling as the other specs, plus the command this tool runs with
    format!(
        r#"{{"Image":{{"environment":{{"K8s":{{"image":"{image}","scheduling":{{"resources":{{"cpu":500,"memory":512}},"spawn_limit":"Unlimited"}}}}}},"execution":{{"args":{{"command":["-e","l"]}}}}}}}}"#
    )
}

/// A tool with `image_from` runs the image of the tool or base build it names with its own
/// command, and has a build that is never built
#[test]
fn tools_reuse_the_image_of_a_tool_or_a_build() {
    // the tool toolbox, a base build, and two tools that reuse scan and the base
    let scratch = tool_toolbox();
    scratch.write("bases/base/manifest.toml", &build_manifest(""));
    scratch.write(
        "tools/scan-le/manifest.toml",
        &reusing_manifest("scan-le", "scan", "latest"),
    );
    scratch.write("tools/scan-le/scan-le.json", &reusing_spec(""));
    scratch.write(
        "tools/on-base/manifest.toml",
        &reusing_manifest("on-base", "base", "1.0"),
    );
    scratch.write("tools/on-base/on-base.json", &reusing_spec(""));
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let manifest = scratch.json("toolbox.json");
    // the reused image is in the spec and is the only tag of a build that is never built
    for (tool, image) in [
        ("scan-le", "registry.test/org/scan:latest"),
        ("on-base", "registry.test/org/base:1.0"),
    ] {
        let version = &manifest["tools"][tool]["versions"]["latest"];
        assert_eq!(
            version["spec"]["Image"]["environment"]["K8s"]["image"], image,
            "{version}"
        );
        assert_eq!(version["build"]["build_image"], false, "{version}");
        assert_eq!(version["build"]["image_tags"], serde_json::json!([image]));
        // the tool keeps its own command
        assert_eq!(
            version["spec"]["Image"]["execution"]["args"]["command"],
            serde_json::json!(["-e", "l"])
        );
    }
    // building the images builds scan and the base, but never the tools that reuse them
    let docker = FakeDocker::new();
    let plan = docker.build_images(&scratch.0, &["toolbox.json", "--dry-run"]);
    assert!(plan.status.success(), "{}", stderr(&plan));
    let planned = stdout(&plan);
    assert!(planned.contains("Would build scan@latest"), "{planned}");
    assert!(planned.contains("Would build base@1.0"), "{planned}");
    assert!(!planned.contains("Would build scan-le"), "{planned}");
    assert!(!planned.contains("Would build on-base"), "{planned}");
}

/// A reusing tool of a refusal case: its name, the item it reuses, that item's version, and the
/// image its spec names
type ReusingTool<'a> = (&'a str, &'a str, &'a str, &'a str);

/// A tool with `image_from` is refused when its spec names an image too, when it names no tool
/// or build of the toolbox, and when the reuses form a cycle
#[test]
fn tools_reusing_an_image_are_refused_when_ambiguous_or_unresolvable() {
    // each case is the reusing tools (name, reused item, its version, the spec's image) and the
    // error expected
    let cases: [(&[ReusingTool], &str); 3] = [
        (
            &[("scan-le", "scan", "latest", "reg/own:1")],
            "leave the spec's image empty",
        ),
        (
            &[("scan-le", "nope", "latest", "")],
            "no tool or build manifest is 'nope@latest'",
        ),
        (
            &[("a", "b", "latest", ""), ("b", "a", "latest", "")],
            "cycles back",
        ),
    ];
    for (tools, expected) in cases {
        let scratch = tool_toolbox();
        for (name, from, version, image) in tools {
            scratch.write(
                &format!("tools/{name}/manifest.toml"),
                &reusing_manifest(name, from, version),
            );
            scratch.write(&format!("tools/{name}/{name}.json"), &reusing_spec(image));
        }
        let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
        assert!(!output.status.success(), "{expected}");
        assert!(
            stderr(&output).contains(expected),
            "{expected}: {}",
            stderr(&output)
        );
    }
}

/// A legacy build writes pipeline configs in the legacy form, without the empty `stage_tools`
/// map only the current format uses, and with the pipeline's `description.md`
#[test]
fn legacy_build_writes_legacy_pipeline_configs() {
    let scratch = legacy_toolbox();
    scratch.write("pipelines/scan/description.md", "Scans files\n");
    let output = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let pipeline = &scratch.json("toolbox.json")["pipelines"]["scan"]["latest"];
    assert!(
        pipeline["config"].get("stage_tools").is_none(),
        "{pipeline}"
    );
    assert_eq!(pipeline["description"], "Scans files");
}

/// The repository's `tools/` reproduces its committed toolbox: building the directory with the
/// settings the tool images are tagged with (`--use-image-path`) writes `tools/toolbox.json` byte
/// for byte, and rewriting the manifests again changes nothing because every manifest that can be
/// a tool already is one
#[test]
fn real_tools_reproduce_the_committed_toolboxes() {
    // a copy of tools/, so the rewrite never touches the repository
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools");
    let scratch = Scratch::new();
    copy_dir(&tools, &scratch.path("tools"));
    let dir = scratch.path("tools");
    let committed = std::fs::read(tools.join("toolbox.json")).expect("read toolbox.json");
    let written = |name: &str| std::fs::read(dir.join(name)).expect("read written toolbox");
    let run = |args: &[&str]| {
        let output = toolbox(&scratch.0, &dir, args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    };
    // building the directory writes the committed toolbox
    run(&[
        "build",
        "-c",
        "config.toml",
        "--use-image-path",
        "-o",
        "rebuilt.json",
    ]);
    assert!(
        written("rebuilt.json") == committed,
        "tools/toolbox.json is stale: rebuild it with `thorctl toolbox build -c config.toml \
         --use-image-path -o toolbox.json` in tools/"
    );
    // rewriting the manifests again writes the same toolbox and keeps no backups
    run(&[
        "build",
        "-c",
        "config.toml",
        "--use-image-path",
        "--rewrite-manifests",
        "-o",
        "rewritten.json",
    ]);
    assert!(
        written("rewritten.json") == committed,
        "rewriting the manifests of tools/ changes the toolbox it builds"
    );
    let backups: Vec<PathBuf> = walk(&dir)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name == "manifest.toml.bak")
        })
        .collect();
    assert!(backups.is_empty(), "manifests left to rewrite: {backups:?}");
}

/// `--rewrite-manifests` refuses to replace an existing backup
#[test]
fn rewrite_manifests_refuses_an_existing_backup() {
    let scratch = legacy_toolbox();
    scratch.write("images/scan/manifest.toml.bak", "keep me\n");
    let output = toolbox(
        &scratch.0,
        &scratch.0,
        &["build", "-c", "config.toml", "--rewrite-manifests"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("manifest.toml.bak"),
        "{}",
        stderr(&output)
    );
    assert_eq!(scratch.read("images/scan/manifest.toml.bak"), "keep me\n");
}

/// `--rewrite-manifests` cannot be combined with `--format-version 1`, and writes nothing
#[test]
fn rewrite_manifests_conflicts_with_format_one() {
    let scratch = legacy_toolbox();
    let output = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "build",
            "-c",
            "config.toml",
            "--rewrite-manifests",
            "--format-version",
            "1",
        ],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--format-version 1"),
        "{}",
        stderr(&output)
    );
}

/// Scaffold one kind of tool with `init tool`
///
/// # Arguments
///
/// * `scratch` - The directory to scaffold under
/// * `kind` - The `--kind` to scaffold
/// * `path` - The directory to scaffold into
fn init_tool(scratch: &Scratch, kind: &str, path: &str) -> Output {
    // scaffold without prompts
    toolbox(
        &scratch.0,
        &scratch.0,
        &["init", "tool", path, "--kind", kind, "-g", "static", "-n"],
    )
}

/// Every kind of `init tool` scaffold builds into a toolbox that validates
#[test]
fn init_tool_scaffolds_build_and_validate() {
    let scratch = Scratch::new();
    scratch.write("config.toml", CONFIG);
    for (kind, path, file) in [
        ("image", "tools/img", "img.json"),
        ("agentic", "tools/agentic", "agentic.json"),
        ("harness", "tools/harness", "harness.json"),
        ("toolset", "toolsets/kit", "kit.toolset.yaml"),
        ("agent", "agents/triage", "triage.agent.yaml"),
        ("workflow", "workflows/flow", "flow.workflow.yaml"),
    ] {
        let output = init_tool(&scratch, kind, path);
        assert!(output.status.success(), "{kind}: {}", stderr(&output));
        assert!(
            scratch.path(&format!("{path}/manifest.toml")).exists(),
            "{kind}"
        );
        assert!(scratch.path(&format!("{path}/{file}")).exists(), "{kind}");
    }
    // an image tool is scaffolded with a build context and its own Dockerfile hint
    let manifest = scratch.read("tools/img/manifest.toml");
    assert!(manifest.contains("type = \"tool\""), "{manifest}");
    assert!(manifest.contains("kind = \"image\""), "{manifest}");
    assert!(manifest.contains("build_path = \"./\""), "{manifest}");
    // the scaffolds build and validate as they are
    let built = toolbox(&scratch.0, &scratch.0, &["build", "-c", "config.toml"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let manifest = scratch.json("toolbox.json");
    assert_eq!(manifest["tools"]["img"]["kind"], "Image");
    assert!(manifest["tools"]["agentic"]["versions"]["latest"]["spec"]["Agentic"].is_object());
    assert!(manifest["tools"]["harness"]["versions"]["latest"]["spec"]["Harness"].is_object());
    assert!(manifest["toolsets"]["kit"].is_object());
    assert!(manifest["agents"]["triage"].is_object());
    assert!(manifest["workflows"]["flow"].is_object());
    let validated = toolbox(&scratch.0, &scratch.0, &["validate", "toolbox.json"]);
    assert!(
        validated.status.success(),
        "{}{}",
        stdout(&validated),
        stderr(&validated)
    );
}

/// `init tool` skips existing files unless `--overwrite` is given, and needs a kind
#[test]
fn init_tool_keeps_existing_files_and_needs_a_kind() {
    let scratch = Scratch::new();
    let first = init_tool(&scratch, "image", "tools/img");
    assert!(first.status.success(), "{}", stderr(&first));
    scratch.write("tools/img/img.json", "custom\n");
    let second = init_tool(&scratch, "image", "tools/img");
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(scratch.read("tools/img/img.json"), "custom\n");
    let third = toolbox(
        &scratch.0,
        &scratch.0,
        &[
            "init",
            "tool",
            "tools/img",
            "--kind",
            "image",
            "-g",
            "static",
            "-n",
            "--overwrite",
        ],
    );
    assert!(third.status.success(), "{}", stderr(&third));
    assert_ne!(scratch.read("tools/img/img.json"), "custom\n");
    // a missing kind is a usage error
    let missing = toolbox(
        &scratch.0,
        &scratch.0,
        &["init", "tool", "tools/x", "-g", "static", "-n"],
    );
    assert_eq!(missing.status.code(), Some(2));
}

/// The directory holding a fake `docker` that records its arguments
struct FakeDocker {
    /// The scratch directory holding the script and its log
    scratch: Scratch,
}

impl FakeDocker {
    /// Create a fake `docker` script that logs every call and succeeds
    fn new() -> Self {
        // the script appends its arguments, one call per line, to a log file
        let scratch = Scratch::new();
        scratch.write(
            "bin/docker",
            "#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/../docker.log\"\nexit 0\n",
        );
        let script = scratch.path("bin/docker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("make the script executable");
        }
        FakeDocker { scratch }
    }

    /// Run `thorctl toolbox build-images` with the fake `docker` first on the `PATH`
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to run in
    /// * `args` - The arguments after `build-images`
    fn build_images(&self, dir: &Path, args: &[&str]) -> Output {
        // put the fake runtime first so it is the one detected
        let path = format!(
            "{}:{}",
            self.scratch.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_thorctl"))
            .args([
                "--config",
                "/nonexistent/thorium.yml",
                "--skip-update",
                "toolbox",
                "build-images",
            ])
            .args(args)
            .current_dir(dir)
            .env("HOME", &self.scratch.0)
            .env("PATH", path)
            .output()
            .expect("run thorctl")
    }

    /// Get the runtime calls that built, tagged, or pushed an image, in order
    fn calls(&self) -> Vec<String> {
        // the runtime's `--version` probe is not a call the command made on purpose
        let log = std::fs::read_to_string(self.scratch.path("docker.log")).unwrap_or_default();
        log.lines()
            .filter(|line| !line.starts_with("--version"))
            .map(str::to_owned)
            .collect()
    }
}

/// A v2 toolbox with base builds, two tools, and a tool that is not built
const V2_BUILDS: &str = r#"{
  "format_version": 2,
  "name": "builds",
  "tools": {
    "alpha": {"groups": ["g"], "versions": {"latest": {
      "build": {"build_path": "ctx/alpha", "build_image": true,
                "image_tags": ["reg/alpha:latest", "mirror/alpha:latest"]}}}},
    "beta": {"groups": ["g"], "versions": {"1.0": {
      "build": {"build_path": "ctx/beta", "build_image": true, "image_tags": ["reg/beta:1.0"],
                "base_image": {"image": "reg/base:2", "image_arg": "BASE"}}}}},
    "gamma": {"groups": ["g"], "versions": {"latest": {
      "build": {"build_path": "ctx/gamma", "build_image": false, "image_tags": ["reg/gamma:latest"]}}}},
    "delta": {"groups": ["g"], "versions": {"latest": {
      "spec": {"Image": {"environment": "External"}}}}}
  },
  "builds": {
    "base": {
      "2": {"build_path": "ctx/base2", "build_image": true, "image_tags": ["reg/base:2"]},
      "1": {"build_path": "ctx/base1", "build_image": true, "image_tags": ["reg/base:1"]}
    }
  }
}
"#;

/// Building images from a v2 toolbox builds the base contexts first, then each tool, aliasing
/// and skipping as the entries say
#[test]
fn build_images_reads_v2_builds_base_first() {
    let docker = FakeDocker::new();
    let scratch = Scratch::new();
    scratch.write("toolbox.json", V2_BUILDS);
    let output = docker.build_images(&scratch.0, &["toolbox.json"]);
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = docker.calls();
    let builds: Vec<&String> = calls
        .iter()
        .filter(|call| call.starts_with("build"))
        .collect();
    // the base contexts come first, in label order, then the tools by name
    assert_eq!(builds.len(), 4, "{calls:?}");
    assert!(builds[0].contains("-t reg/base:1"), "{calls:?}");
    assert!(builds[1].contains("-t reg/base:2"), "{calls:?}");
    assert!(builds[2].contains("-t reg/alpha:latest"), "{calls:?}");
    assert!(builds[3].contains("-t reg/beta:1.0"), "{calls:?}");
    // the second tag of alpha is an alias, and beta takes its base image build arg
    assert!(
        calls.contains(&"tag reg/alpha:latest mirror/alpha:latest".to_owned()),
        "{calls:?}"
    );
    assert!(
        builds[3].contains("--build-arg BASE=reg/base:2"),
        "{calls:?}"
    );
    // the entry with building disabled is skipped
    assert!(
        !calls.iter().any(|call| call.contains("gamma")),
        "{calls:?}"
    );
    assert!(
        stdout(&output).contains("gamma@latest"),
        "{}",
        stdout(&output)
    );
}

/// `--tools` narrows the build to the named tools and base builds, and `--images` still works
#[test]
fn build_images_filters_by_name() {
    let docker = FakeDocker::new();
    let scratch = Scratch::new();
    scratch.write("toolbox.json", V2_BUILDS);
    for flag in ["--tools", "--images", "-i", "-t"] {
        let output = docker.build_images(&scratch.0, &["toolbox.json", flag, "base,beta"]);
        assert!(
            output.status.success(),
            "{flag}: {}{}",
            stdout(&output),
            stderr(&output)
        );
    }
    let calls = docker.calls();
    // each of the four runs built the two base contexts and beta
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("build"))
            .count(),
        12,
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("alpha")),
        "{calls:?}"
    );
}

/// A toolbox directory is accepted in place of the manifest path
#[test]
fn build_images_accepts_a_directory() {
    let docker = FakeDocker::new();
    let scratch = Scratch::new();
    scratch.write("tb/toolbox.json", V2_BUILDS);
    let output = docker.build_images(&scratch.0, &["tb"]);
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        docker
            .calls()
            .iter()
            .filter(|call| call.starts_with("build"))
            .count(),
        4
    );
}

/// `--dry-run` prints the plan in build order and never calls the runtime
#[test]
fn build_images_dry_run_prints_the_plan() {
    let docker = FakeDocker::new();
    let scratch = Scratch::new();
    scratch.write("toolbox.json", V2_BUILDS);
    let output = docker.build_images(
        &scratch.0,
        &["toolbox.json", "--dry-run", "--push", "--tag-suffix", "-br"],
    );
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(docker.calls().is_empty(), "{:?}", docker.calls());
    let text = stdout(&output);
    let order: Vec<usize> = ["base@1", "base@2", "alpha@latest", "beta@1.0"]
        .iter()
        .map(|label| {
            text.find(label)
                .unwrap_or_else(|| panic!("{label} missing from {text}"))
        })
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    assert!(text.contains("reg/alpha:latest-br"), "{text}");
    assert!(text.contains("mirror/alpha:latest-br"), "{text}");
    assert!(text.contains("Skipping gamma@latest"), "{text}");
}

/// A legacy toolbox still builds its images in name order
#[test]
fn build_images_still_reads_legacy_toolboxes() {
    let docker = FakeDocker::new();
    let scratch = Scratch::new();
    scratch.write(
        "toolbox.json",
        r#"{"images": {
            "b": {"latest": {"build_path": "b", "build_image": true, "image_tags": ["reg/b:latest"], "config": {}}},
            "a": {"latest": {"build_path": "a", "build_image": true, "image_tags": ["reg/a:latest"]}}
        }}"#,
    );
    let output = docker.build_images(&scratch.0, &["toolbox.json"]);
    assert!(
        output.status.success(),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    let calls = docker.calls();
    assert!(
        calls[0].contains("reg/a:latest") && calls[1].contains("reg/b:latest"),
        "{calls:?}"
    );
}

/// A manifest with nothing to build is an error naming the sections it looked in
#[test]
fn build_images_needs_something_to_build() {
    let docker = FakeDocker::new();
    let scratch = Scratch::new();
    scratch.write("toolbox.json", r#"{"format_version": 2, "name": "empty"}"#);
    let output = docker.build_images(&scratch.0, &["toolbox.json"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("No build entries"),
        "{}",
        stderr(&output)
    );
}

/// The repository's toolboxes plan the same builds in both formats, with the four base
/// contexts first in the current one
#[test]
fn the_repository_toolboxes_plan_the_same_builds() {
    let docker = FakeDocker::new();
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools");
    let plan = |file: &str| {
        let output = docker.build_images(&tools, &[file, "--dry-run"]);
        assert!(
            output.status.success(),
            "{}{}",
            stdout(&output),
            stderr(&output)
        );
        stdout(&output)
            .lines()
            .filter(|line| line.contains("Would build"))
            .map(str::to_owned)
            .collect::<Vec<String>>()
    };
    let legacy = plan("toolbox.v1.json");
    let converted = plan("toolbox.json");
    // the current toolbox builds its four ghidra-base contexts before any tool
    for line in &converted[..4] {
        assert!(line.contains("ghidra-base@"), "{line}");
    }
    // the same images are built in either format
    let mut left = legacy.clone();
    let mut right = converted.clone();
    left.sort();
    right.sort();
    assert_eq!(left, right);
    assert!(converted.len() > 50, "{}", converted.len());
}
