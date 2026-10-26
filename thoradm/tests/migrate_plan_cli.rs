//! Tests of `thoradm migrate plan` as a process: it needs no cluster, and refuses a downgrade
//! across the tool conversion steps naming the step and lists each revision's steps

use std::process::{Command, Output};

/// Run `thoradm migrate plan` between two revisions
///
/// # Arguments
///
/// * `from` - The revision the cluster is at
/// * `to` - The revision to plan to
fn plan(from: &str, to: &str) -> Output {
    // run the binary cargo built for these tests, which reads no cluster config for a plan
    Command::new(env!("CARGO_BIN_EXE_thoradm"))
        .args(["migrate", "plan", "--from", from, "--to", to])
        .output()
        .expect("thoradm runs")
}

/// Get the step names a plan lists, in the order it lists them
///
/// # Arguments
///
/// * `output` - The output of a plan that succeeded
fn listed(output: &Output) -> Vec<String> {
    // each step line holds the revision and then the step's id
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("  2026-"))
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(ToOwned::to_owned)
        .collect()
}

/// A plan down from the newest revision exits non-zero with a message naming the first tool
/// conversion step, whether it goes back one revision or two
#[test]
fn a_plan_down_across_the_tool_conversions_is_refused_naming_the_step() {
    // the way down is refused and nothing is listed
    for to in ["2026-10-v03", "2026-10-v02"] {
        let output = plan("2026-10-v04", to);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{to}: {stderr}");
        assert!(stderr.contains("images-to-tools"), "{to}: {stderr}");
        assert!(stderr.contains("restore from backup"), "{to}: {stderr}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    }
}

/// A plan down from the revision before the newest one still succeeds, listing the users reverse
/// step and none of the tool conversions
#[test]
fn a_plan_down_from_the_schema_revision_lists_the_users_reverse() {
    // the step that moves users back is the only one listed
    let output = plan("2026-10-v03", "2026-10-v02");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(listed(&output), ["users-settings-theme-reverse"]);
}

/// A plan up lists the steps of each revision it crosses: the first release's alone, the
/// conversions alone from it, and both with the drain before the users step when going straight
#[test]
fn a_plan_up_lists_the_steps_of_each_revision_it_crosses() {
    // the revision that applies the schema first holds no conversion step
    let output = plan("2026-10-v02", "2026-10-v03");
    assert!(output.status.success());
    assert_eq!(listed(&output), ["schema-apply", "users-settings-theme"]);
    // the newest revision holds the drain and the conversions, with the record last
    let conversions = [
        "drain-reactions",
        "images-to-tools",
        "pipelines-to-tool-stages",
        "provenance-to-legacy-tools",
        "finalize-tool-only",
    ];
    let output = plan("2026-10-v03", "2026-10-v04");
    assert!(output.status.success());
    assert_eq!(listed(&output), conversions);
    // straight to it, the drain comes before the users step that pauses the API
    let output = plan("2026-10-v02", "2026-10-v04");
    assert!(output.status.success());
    assert_eq!(
        listed(&output),
        [
            "schema-apply",
            "drain-reactions",
            "users-settings-theme",
            "images-to-tools",
            "pipelines-to-tool-stages",
            "provenance-to-legacy-tools",
            "finalize-tool-only",
        ]
    );
}
