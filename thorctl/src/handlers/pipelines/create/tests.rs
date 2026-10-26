//! Unit tests for the pipeline create command

use clap::Parser;
use thorium::models::EventTrigger;

use super::build_view;
use crate::args::pipelines::CreatePipeline;

/// Parse a create command line
///
/// # Arguments
///
/// * `flags` - The arguments after `create orbit -g jupiter`
fn parse(flags: &[&str]) -> CreatePipeline {
    let mut argv = vec!["create", "orbit", "-g", "jupiter"];
    argv.extend_from_slice(flags);
    CreatePipeline::try_parse_from(argv).unwrap()
}

/// Write a spec file and return its path
///
/// # Arguments
///
/// * `text` - The spec
fn spec_file(text: &str) -> (tempfile_dir::Dir, String) {
    let dir = tempfile_dir::Dir::new();
    let path = dir.path().join("spec.yml");
    std::fs::write(&path, text).unwrap();
    let path = path.to_string_lossy().into_owned();
    (dir, path)
}

/// A small scratch directory that is removed when dropped
mod tempfile_dir {
    use std::path::{Path, PathBuf};

    /// A directory under the system temp dir
    pub struct Dir(PathBuf);

    impl Dir {
        /// Create a unique empty directory
        pub fn new() -> Self {
            let path = std::env::temp_dir().join(format!("thorctl-l5-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }

        /// The directory's path
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        /// Remove the directory
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Stages are steps in the order they are given, and commas run stages together
#[test]
fn stages_keep_their_order_and_grouping() {
    let view = build_view(&parse(&[
        "--stage",
        "fetch",
        "--stage",
        "scan,hash",
        "--stage",
        "report",
        "--stage-tool",
        "fetch=a",
        "--stage-tool",
        "scan=b@1.0.0",
        "--stage-tool",
        "hash=b",
        "--stage-tool",
        "report=c",
    ]))
    .unwrap();
    assert_eq!(
        view.order,
        vec![
            vec!["fetch".to_owned()],
            vec!["scan".to_owned(), "hash".to_owned()],
            vec!["report".to_owned()]
        ]
    );
    assert_eq!(view.stage_tools["scan"].version.as_deref(), Some("1.0.0"));
    view.check().unwrap();
}

/// A stage without a tool is refused, and so is a pipeline without stages
#[test]
fn stages_need_tools() {
    let view = build_view(&parse(&["--stage", "a,b", "--stage-tool", "a=x"])).unwrap();
    let err = view.check().unwrap_err();
    assert!(err.to_string().contains("no tool is set for: b"), "{err}");
    let view = build_view(&parse(&[])).unwrap();
    assert!(view.check().is_err());
}

/// Settings come from the flags: SLA, description and triggers
#[test]
fn settings_come_from_flags() {
    let view = build_view(&parse(&[
        "--stage",
        "a",
        "--stage-tool",
        "a=x",
        "--sla",
        "60",
        "-d",
        "text",
        "--trigger",
        "t=NewSample",
    ]))
    .unwrap();
    assert_eq!(view.sla, Some(60));
    assert_eq!(view.description.as_deref(), Some("text"));
    assert_eq!(view.triggers["t"], EventTrigger::NewSample);
}

/// A spec file describes the pipeline and the flags add to it or replace parts of it
#[test]
fn spec_file_is_extended_by_flags() {
    let (_dir, path) = spec_file(
        "order: [[a], [b]]\nsla: 100\ndescription: from spec\nstage_tools:\n  a: {tool: x}\n  b: {tool: y, version: '1.0.0'}\n",
    );
    // the spec alone is complete
    let view = build_view(&parse(&["--spec", &path])).unwrap();
    view.check().unwrap();
    assert_eq!(view.sla, Some(100));
    assert_eq!(view.stage_tools["b"].version.as_deref(), Some("1.0.0"));
    // flags replace a stage's tool and the SLA
    let view = build_view(&parse(&[
        "--spec",
        &path,
        "--stage-tool",
        "b=z",
        "--sla",
        "5",
    ]))
    .unwrap();
    assert_eq!(view.stage_tools["b"].tool, "z");
    assert_eq!(view.stage_tools["b"].version, None);
    assert_eq!(view.sla, Some(5));
    assert_eq!(view.description.as_deref(), Some("from spec"));
    // stages from the flags replace the spec's order
    let view = build_view(&parse(&["--spec", &path, "--stage", "b"])).unwrap();
    assert_eq!(view.order, vec![vec!["b".to_owned()]]);
}

/// A spec with an unknown key or a missing file is refused before anything is requested
#[test]
fn bad_specs_are_refused() {
    let (_dir, path) = spec_file("order: [[a]]\nstages_tools: {}\n");
    let err = build_view(&parse(&["--spec", &path])).unwrap_err();
    assert!(err.to_string().contains("Invalid pipeline spec"), "{err}");
    assert!(build_view(&parse(&["--spec", "/nonexistent/spec.yml"])).is_err());
}
