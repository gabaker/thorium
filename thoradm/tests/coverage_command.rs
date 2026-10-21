//! Tests of `thoradm backup coverage`, which prints the backup coverage registry from the binary
//! with no config file and no backing service

use std::process::{Command, Output};
use thoradm::backup::coverage::{COVERAGE, Coverage, Registry};

/// Runs `thoradm` with arguments that point at no config, no cluster, and no backend
///
/// # Arguments
///
/// * `args` - The arguments after the global flags
fn run(args: &[&str]) -> Output {
    // aim the config flags and the config environment at nothing that exists or answers
    Command::new(env!("CARGO_BIN_EXE_thoradm"))
        .args([
            "--cluster-conf",
            "/nonexistent/thorium.yml",
            "--ctl-conf",
            "/nonexistent/config.yml",
        ])
        .args(args)
        .env("THORIUM__SCYLLA__NODES", "127.0.0.1:1")
        .env("THORIUM__REDIS__HOST", "127.0.0.1")
        .env("THORIUM__THORIUM__S3__ENDPOINT", "http://127.0.0.1:1")
        .output()
        .expect("thoradm should start")
}

/// The default output is the coverage page, byte for byte, and nothing else is written
#[test]
fn the_command_prints_the_page_the_example_prints() {
    // run the subcommand with no config and no backend
    let output = run(&["backup", "coverage"]);
    assert!(output.status.success(), "{output:?}");
    // stdout is exactly the page the shared renderer produces
    assert_eq!(
        String::from_utf8(output.stdout.clone()).unwrap(),
        Registry::render_page()
    );
    // nothing was logged, so no connection or config load was attempted
    assert!(output.stderr.is_empty(), "{output:?}");
}

/// `--json` prints one object per registry entry, once each and in registry order
#[test]
fn json_lists_every_entry_once_in_registry_order() {
    // run the subcommand with --json and parse its stdout
    let output = run(&["backup", "coverage", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let entries = parsed.as_array().expect("a JSON array");
    assert_eq!(entries.len(), COVERAGE.len());
    // each element describes the registry entry at its position
    for (json, entry) in entries.iter().zip(COVERAGE) {
        assert_eq!(json["name"], entry.name);
        let store = serde_json::to_value(entry.store).unwrap();
        assert_eq!(json["store"], store, "{}", entry.name);
        let category = serde_json::to_value(entry.category).unwrap();
        assert_eq!(json["category"], category, "{}", entry.name);
        assert_eq!(
            json["gap"],
            entry.gap.map_or(serde_json::Value::Null, Into::into),
            "{}",
            entry.name
        );
        assert_eq!(
            json["prefixes"].as_array().unwrap().len(),
            entry.prefixes.len(),
            "{}",
            entry.name
        );
        assert_eq!(
            json["base_table"],
            entry.base_table.map_or(serde_json::Value::Null, Into::into),
            "{}",
            entry.name
        );
        // a backed-up entry names its component, an excluded one its reason
        match entry.coverage {
            Coverage::BackedUp(component) => {
                assert_eq!(json["classification"], "backed-up", "{}", entry.name);
                assert_eq!(json["component"], component.to_string(), "{}", entry.name);
                assert!(json["reason"].is_null(), "{}", entry.name);
            }
            Coverage::Excluded { reason } => {
                assert_eq!(json["classification"], "excluded", "{}", entry.name);
                assert_eq!(json["reason"], reason, "{}", entry.name);
                assert!(json["component"].is_null(), "{}", entry.name);
            }
        }
    }
}

/// `--help` lists the subcommand and its exit codes
#[test]
fn help_names_the_command_and_its_exit_codes() {
    // the backup help lists the subcommand
    let backup = run(&["backup", "--help"]);
    assert!(
        String::from_utf8_lossy(&backup.stdout).contains("coverage"),
        "{backup:?}"
    );
    // the subcommand's help names its flag and exit codes
    let help = run(&["backup", "coverage", "--help"]);
    let text = String::from_utf8_lossy(&help.stdout).into_owned();
    assert!(
        text.contains("--json") && text.contains("Exit codes"),
        "{text}"
    );
}
