//! Env-gated live-bridge test for multi-program sessions.
//!
//! Exercises the whole import-child path against a real Ghidra install with no
//! model calls and no malware: a scratch project is bootstrapped over one
//! benign binary, a second benign binary is imported as a child, and the
//! program-pinning lock is driven from both the shared and exclusive sides.
//!
//! Ignored by default because it needs a Ghidra install; run it with:
//!
//! ```text
//! THORPAL_LIVE_GHIDRA=<install dir> cargo test -p thorpal --test child_import_live -- --ignored
//! ```

use std::path::PathBuf;

use thorpal::{GhidraSession, GhidraSettings};

/// Bootstrap a scratch project, import a child, and pin both programs.
#[tokio::test]
#[ignore = "needs a Ghidra install; set THORPAL_LIVE_GHIDRA to run"]
async fn child_import_round_trip() {
    // the install comes from the environment so CI never needs Ghidra
    let install = match std::env::var("THORPAL_LIVE_GHIDRA") {
        Ok(install) => PathBuf::from(install),
        Err(_) => panic!("set THORPAL_LIVE_GHIDRA to the Ghidra install dir"),
    };
    // stage two benign ELF binaries under a scratch, dot-free projects root
    let scratch = tempfile::Builder::new()
        .prefix("thorpal-live")
        .tempdir_in("/tmp")
        .expect("scratch dir should create");
    let main_bin = scratch.path().join("main-sample");
    let child_bin = scratch.path().join("child-sample");
    std::fs::copy("/bin/true", &main_bin).expect("main copy should write");
    std::fs::copy("/bin/false", &child_bin).expect("child copy should write");
    let projects = scratch.path().join("projects");
    // bootstrap a fresh project over the main binary
    let settings = GhidraSettings::for_analysis(main_bin, projects, Some(install), true)
        .expect("settings should resolve");
    let session = GhidraSession::new(settings);
    // import the child under its workspace-style name
    let imported = session
        .import_child(&child_bin, "child-sample")
        .await
        .expect("child import should succeed");
    assert_eq!(imported.program, "child-sample");
    assert!(imported.analyzed);
    // a repeat import is idempotent and reuses the existing program
    let again = session
        .import_child(&child_bin, "child-sample")
        .await
        .expect("re-import should succeed");
    assert!(again.note.contains("already imported"));
    // the exclusive side pins the child; the current program must be it
    {
        let op = session
            .op(Some("child-sample"))
            .await
            .expect("child op should pin");
        let info = op
            .ghidra()
            .program
            .info()
            .await
            .expect("child program info should answer");
        assert_eq!(info.name, "child-sample");
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
    // an unknown program names the known ones instead of switching blindly
    let err = match session.op(Some("no-such-program")).await {
        Ok(_) => panic!("unknown program must not pin"),
        Err(err) => format!("{err:#}"),
    };
    assert!(err.contains("known programs"));
    // shut the bridge down cleanly so the scratch project flushes and releases
    session.shutdown().await.expect("shutdown should flush");
}
