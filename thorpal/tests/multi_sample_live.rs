//! Env-gated live-bridge test for multi-sample session bootstrap.
//!
//! Exercises the sibling-import path against a real Ghidra install with no
//! model calls and no malware: a scratch project is bootstrapped over one
//! benign binary with a second benign binary configured as a sibling sample
//! plus one deliberately unloadable sibling (a text file Ghidra has no loader
//! for), then both loadable programs are pinned from the exclusive and shared
//! sides while the unloadable one must answer with its recorded import
//! failure instead of sinking the whole session (the multi-sample
//! "poison-pill" regression).
//!
//! Ignored by default because it needs a Ghidra install; run it with:
//!
//! ```text
//! THORPAL_LIVE_GHIDRA=<install dir> cargo test -p thorpal --test multi_sample_live -- --ignored
//! ```

use std::path::PathBuf;

use thorpal::{GhidraSession, GhidraSettings};

/// Bootstrap a project with loadable and unloadable siblings and pin programs.
#[tokio::test]
#[ignore = "needs a Ghidra install; set THORPAL_LIVE_GHIDRA to run"]
async fn multi_sample_bootstrap_round_trip() {
    // the install comes from the environment so CI never needs Ghidra
    let install = match std::env::var("THORPAL_LIVE_GHIDRA") {
        Ok(install) => PathBuf::from(install),
        Err(_) => panic!("set THORPAL_LIVE_GHIDRA to the Ghidra install dir"),
    };
    // stage two benign ELF binaries under a scratch, dot-free projects root
    let scratch = tempfile::Builder::new()
        .prefix("thorpal-multi")
        .tempdir_in("/tmp")
        .expect("scratch dir should create");
    let main_bin = scratch.path().join("primary-sample");
    let sibling_bin = scratch.path().join("sibling-sample");
    std::fs::copy("/bin/true", &main_bin).expect("primary copy should write");
    std::fs::copy("/bin/false", &sibling_bin).expect("sibling copy should write");
    // stage a sibling Ghidra has no loader for (plain text -> no load spec);
    // before the poison-pill fix this single file killed Ghidra all session
    let unloadable_bin = scratch.path().join("unloadable-sibling.txt");
    std::fs::write(&unloadable_bin, b"just text, nothing Ghidra can load\n")
        .expect("unloadable sibling should write");
    let projects = scratch.path().join("projects");
    // resolve fresh settings over the primary and attach the siblings, exactly
    // as thorctl's build_config does for a multi-sample invocation
    let mut settings = GhidraSettings::for_analysis(main_bin, projects, Some(install), true)
        .expect("settings should resolve");
    settings.extra_binaries = vec![sibling_bin, unloadable_bin];
    // remember where the project lives so the lock release is checkable later
    let projects_dir = settings.projects_dir.clone();
    let project_name = settings.project_name.clone();
    let session = GhidraSession::new(settings);
    // the first connection bootstraps the project AND imports the siblings;
    // the unloadable one must be tolerated, not fail the whole bootstrap
    let ghidra = session.get().await.expect("bootstrap should connect");
    let programs = ghidra.program.list().await.expect("programs should list");
    let names: Vec<&str> = programs.iter().map(|info| info.name.as_str()).collect();
    let main_name = session.main_program_name();
    assert!(
        names.contains(&main_name.as_str()),
        "main program missing from {names:?}"
    );
    assert!(
        names.contains(&"sibling-sample"),
        "sibling program missing from {names:?}"
    );
    // the unloadable sibling never became a program, only a recorded failure
    assert!(
        !names.contains(&"unloadable-sibling.txt"),
        "the unloadable sibling must not appear as a program: {names:?}"
    );
    let reason = session
        .failed_import("unloadable-sibling.txt")
        .expect("the unloadable sibling's failure should be recorded");
    assert!(!reason.is_empty());
    // the loadability gate classified the text head and skipped the import
    // outright -- a paid (failed) import would have recorded the real
    // "No load spec found" chain here instead
    assert!(
        reason.starts_with("not attempted:"),
        "the gate should record a synthetic reason, got: {reason}"
    );
    // pinning the failed name answers with the recorded guidance, not a hang
    let err = match session.op(Some("unloadable-sibling.txt")).await {
        Ok(_) => panic!("pinning the unloadable sibling should error"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        err.contains("failed during session bootstrap"),
        "unexpected failed-import error: {err}"
    );
    // the exclusive side pins the sibling; the current program must be it
    {
        let op = session
            .op(Some("sibling-sample"))
            .await
            .expect("sibling op should pin");
        let info = op
            .ghidra()
            .program
            .info()
            .await
            .expect("sibling program info should answer");
        assert_eq!(info.name, "sibling-sample");
        assert!(info.function_count.unwrap_or_default() > 0);
    }
    // the shared side heals back to the main program afterwards
    {
        let op = session.op(None).await.expect("main op should pin");
        let info = op
            .ghidra()
            .program
            .info()
            .await
            .expect("main program info should answer");
        assert_eq!(info.name, session.main_program_name());
    }
    // a second bootstrap-equivalent call is idempotent across the same session
    let ghidra = session
        .get()
        .await
        .expect("the cached handle should return");
    let count = ghidra
        .program
        .list()
        .await
        .expect("programs should list again")
        .len();
    assert_eq!(count, 2, "re-ensuring must not duplicate programs");
    // shut the bridge down cleanly so the scratch project flushes and releases
    session.shutdown().await.expect("shutdown should flush");
    // a clean shutdown releases the project lock; a survivor would block the
    // analysis-dir rename and the next session over this project
    let lock = projects_dir.join(format!("{project_name}.lock"));
    assert!(
        !lock.exists(),
        "the project lock should be released by shutdown: {}",
        lock.display()
    );
}

/// A non-loadable primary no longer sinks the session: a sibling seeds the project.
#[tokio::test]
#[ignore = "needs a Ghidra install; set THORPAL_LIVE_GHIDRA to run"]
async fn unloadable_primary_falls_back_to_a_sibling() {
    // the install comes from the environment so CI never needs Ghidra
    let install = match std::env::var("THORPAL_LIVE_GHIDRA") {
        Ok(install) => PathBuf::from(install),
        Err(_) => panic!("set THORPAL_LIVE_GHIDRA to the Ghidra install dir"),
    };
    // stage an unloadable primary (plain text) with one loadable ELF sibling
    let scratch = tempfile::Builder::new()
        .prefix("thorpal-primary")
        .tempdir_in("/tmp")
        .expect("scratch dir should create");
    let main_bin = scratch.path().join("dropper-doc.txt");
    std::fs::write(&main_bin, b"just text, nothing Ghidra can load\n")
        .expect("primary should write");
    let sibling_bin = scratch.path().join("payload-sample");
    std::fs::copy("/bin/true", &sibling_bin).expect("sibling copy should write");
    let projects = scratch.path().join("projects");
    // resolve fresh settings over the unloadable primary with the sibling
    let mut settings = GhidraSettings::for_analysis(main_bin, projects, Some(install), true)
        .expect("settings should resolve");
    settings.extra_binaries = vec![sibling_bin];
    let session = GhidraSession::new(settings);
    // bootstrap succeeds by seeding the project from the loadable sibling
    let ghidra = session.get().await.expect("bootstrap should connect");
    let main_name = session.main_program_name();
    // the primary is recorded as gated: the loadability gate classified the
    // text head and never paid the doomed import at all
    let reason = session
        .failed_import(&main_name)
        .expect("the primary's import failure should be recorded");
    assert!(!reason.is_empty());
    assert!(
        reason.starts_with("not attempted:"),
        "the gate should record a synthetic reason, got: {reason}"
    );
    // the sibling is a live, pinnable program
    {
        let op = session
            .op(Some("payload-sample"))
            .await
            .expect("the sibling should pin");
        let info = op
            .ghidra()
            .program
            .info()
            .await
            .expect("sibling program info should answer");
        assert_eq!(info.name, "payload-sample");
    }
    // a main-program call answers with the recorded guidance, never a heal loop
    let err = match session.op(None).await {
        Ok(_) => panic!("pinning the unloadable primary should error"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        err.contains("failed during session bootstrap"),
        "unexpected main-program error: {err}"
    );
    // the project holds exactly the sibling
    let names: Vec<String> = ghidra
        .program
        .list()
        .await
        .expect("programs should list")
        .into_iter()
        .map(|info| info.name)
        .collect();
    assert_eq!(names, vec!["payload-sample".to_owned()]);
    // shut the bridge down cleanly so the scratch project flushes and releases
    session.shutdown().await.expect("shutdown should flush");
}

/// A set with no loadable member never starts a Ghidra JVM at all.
#[tokio::test]
#[ignore = "needs a Ghidra install; set THORPAL_LIVE_GHIDRA to run"]
async fn whole_set_unloadable_never_boots_a_jvm() {
    // the install comes from the environment so CI never needs Ghidra
    let install = match std::env::var("THORPAL_LIVE_GHIDRA") {
        Ok(install) => PathBuf::from(install),
        Err(_) => panic!("set THORPAL_LIVE_GHIDRA to the Ghidra install dir"),
    };
    // stage a script primary and a zip-headed sibling: nothing Ghidra can load
    let scratch = tempfile::Builder::new()
        .prefix("thorpal-unloadable")
        .tempdir_in("/tmp")
        .expect("scratch dir should create");
    let main_bin = scratch.path().join("dropper.vbs");
    std::fs::write(
        &main_bin,
        b"On Error Resume Next\r\nDim shell\r\nSet shell = \
          CreateObject(\"WScript.Shell\")\r\n",
    )
    .expect("primary should write");
    let sibling_bin = scratch.path().join("payload.zip");
    std::fs::write(
        &sibling_bin,
        b"PK\x03\x04not really a zip, but the magic rules",
    )
    .expect("sibling should write");
    let projects = scratch.path().join("projects");
    // resolve fresh settings over the all-unloadable set
    let mut settings = GhidraSettings::for_analysis(main_bin, projects, Some(install), true)
        .expect("settings should resolve");
    settings.extra_binaries = vec![sibling_bin];
    // remember where a project WOULD live so its absence is checkable
    let projects_dir = settings.projects_dir.clone();
    let project_name = settings.project_name.clone();
    let session = GhidraSession::new(settings);
    // the classification gate fails the bootstrap in milliseconds, without
    // ever spawning analyzeHeadless (which takes tens of seconds per member)
    let start = std::time::Instant::now();
    let err = match session.get().await {
        Ok(_) => panic!("an all-unloadable set must not connect"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        err.contains("Ghidra was not started"),
        "unexpected whole-set error: {err}"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "the gate must answer without paying a JVM attempt"
    );
    // every member is recorded as gated, never as a paid failure
    for name in [session.main_program_name(), "payload.zip".to_owned()] {
        let reason = session
            .failed_import(&name)
            .unwrap_or_else(|| panic!("'{name}' should carry a gate record"));
        assert!(
            reason.starts_with("not attempted:"),
            "'{name}' should be gated, got: {reason}"
        );
    }
    // no project exists; note a paid-and-failed seed import also cleans up
    // its leftover .gpr, so the real no-attempt proof is the elapsed-time
    // bound plus the "Ghidra was not started" (gated, last_error=None) text
    // above -- this check guards the no-project invariant itself
    let gpr = projects_dir.join(format!("{project_name}.gpr"));
    assert!(
        !gpr.exists(),
        "no project may exist for an all-unloadable set: {}",
        gpr.display()
    );
    // a loadable payload carved out of the unloadable set seeds the project:
    // the carve-and-import escalation the seeded questions prescribe works
    // even though no original sample ever loaded
    let carved = scratch.path().join("carved-payload");
    std::fs::copy("/bin/true", &carved).expect("carved payload should write");
    let seeded = session
        .import_child(&carved, "carved-payload")
        .await
        .expect("a carved native executable should seed the project");
    assert!(
        seeded.note.contains("seeded the Ghidra project"),
        "unexpected note: {}",
        seeded.note
    );
    // the project now exists and the child is a live, pinnable program
    assert!(gpr.exists(), "the carved import should create the project");
    {
        let op = session
            .op(Some(&seeded.program))
            .await
            .expect("the carved program should pin");
        let info = op
            .ghidra()
            .program
            .info()
            .await
            .expect("carved program info should answer");
        assert_eq!(info.name, seeded.program);
    }
    // main-program calls still answer from the gated record, never a heal loop
    let err = match session.op(None).await {
        Ok(_) => panic!("the gated primary must stay unavailable"),
        Err(err) => format!("{err:#}"),
    };
    assert!(err.contains("not attempted"), "unexpected error: {err}");
    // teardown now stops a real bridge and must release the project lock
    session.shutdown().await.expect("shutdown should flush");
    let lock = projects_dir.join(format!("{project_name}.lock"));
    assert!(
        !lock.exists(),
        "the project lock should be released by shutdown: {}",
        lock.display()
    );
}
