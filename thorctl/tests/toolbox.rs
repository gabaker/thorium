//! Tests the thorctl toolbox commands that need no API

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::unique;
use thorium::Error;
use tokio::net::TcpListener;
use tokio::process::Command;

/// Write a file into a new temp directory and return its path
///
/// # Arguments
///
/// * `name` - The file's name
/// * `contents` - The file's contents
fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
    // make a unique directory for this file
    let dir = std::env::temp_dir().join(unique("thorctl-toolbox-"));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("the file");
    path
}

/// `toolbox import` refuses every manifest before it talks to the API, pointing to the tool
/// commands
#[tokio::test]
async fn toolbox_import_fails_fast() -> Result<(), Error> {
    // an API stand-in that counts the connections it gets
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        while let Ok((_, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    // a keys file pointing thorctl at the stand-in
    let keys = temp_file(
        "keys.yml",
        &format!("api: http://{addr}\nusername: europa\npassword: hunter2\n"),
    );
    // a manifest with legacy images, one with only pipelines, and a toolbox directory
    let images = temp_file(
        "toolbox.json",
        r#"{"name":"moons","version":"1.0.0","images":[{"name":"scan","group":"jupiter"}]}"#,
    );
    let pipelines = temp_file(
        "toolbox.json",
        r#"{"name":"moons","version":"1.0.0","pipelines":[{"name":"orbit","group":"jupiter"}]}"#,
    );
    let directory = std::env::temp_dir().join(unique("thorctl-toolbox-dir-"));
    std::fs::create_dir_all(&directory)?;
    for manifest in [&images, &pipelines, &directory] {
        // import the manifest
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            Command::new(env!("CARGO_BIN_EXE_thorctl"))
                .arg("--keys")
                .arg(&keys)
                .arg("--skip-update")
                .arg("toolbox")
                .arg("import")
                .arg(manifest)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .expect("thorctl timed out")?;
        // it fails, naming the tool commands and not the snapshot command
        assert!(!out.status.success(), "{manifest:?} imported");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("thorctl tools create"), "{stderr}");
        assert!(stderr.contains("pipelines stage-tool"), "{stderr}");
        assert!(!stderr.contains("tools snapshot"), "{stderr}");
    }
    // the stand-in got no connection
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    Ok(())
}
