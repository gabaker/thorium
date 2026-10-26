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

/// Run thorctl against an API stand-in and return its output
///
/// # Arguments
///
/// * `keys` - The keys file pointing at the stand-in
/// * `args` - The arguments after `toolbox`
async fn toolbox(keys: &std::path::Path, args: &[&std::path::Path]) -> std::process::Output {
    // run the toolbox command, giving up if it hangs
    tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(env!("CARGO_BIN_EXE_thorctl"))
            .arg("--keys")
            .arg(keys)
            .arg("--skip-update")
            .arg("toolbox")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .expect("thorctl timed out")
    .expect("thorctl ran")
}

/// `toolbox import` and `toolbox update` refuse a toolbox that can't be loaded, or that
/// `--strict` doesn't accept, before they connect to the API
#[tokio::test]
async fn toolbox_import_refuses_bad_toolboxes_before_connecting() -> Result<(), Error> {
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
    // a manifest that isn't one, an empty directory, a missing file, and a legacy toolbox that
    // has an image that can't be converted
    let invalid = temp_file(
        "toolbox.json",
        r#"{"name":"moons","version":"1.0.0","images":[{"name":"scan","group":"jupiter"}]}"#,
    );
    let directory = std::env::temp_dir().join(unique("thorctl-toolbox-dir-"));
    std::fs::create_dir_all(&directory)?;
    let missing = directory.join("nothing.json");
    let legacy = temp_file(
        "toolbox.json",
        include_str!("data/toolbox_v2/small.legacy.json"),
    );
    let refused: [(&str, &std::path::Path, &[&str]); 5] = [
        ("import", &invalid, &[]),
        ("import", &directory, &[]),
        ("import", &missing, &[]),
        ("import", &legacy, &["--strict"]),
        ("update", &legacy, &["--strict"]),
    ];
    for (command, manifest, flags) in refused {
        // run the command
        let mut args: Vec<&std::path::Path> = vec![command.as_ref(), manifest];
        args.extend(flags.iter().map(|flag| std::path::Path::new(*flag)));
        let out = toolbox(&keys, &args).await;
        // it fails and says why
        assert!(
            !out.status.success(),
            "{command} {manifest:?} {flags:?} succeeded"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.trim().is_empty(), "{stderr}");
        assert!(!stderr.contains("not implemented"), "{stderr}");
    }
    // a strict import of the legacy toolbox names the conversion issue it stopped at
    let out = toolbox(
        &keys,
        &["import".as_ref(), legacy.as_ref(), "--strict".as_ref()],
    )
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing_field:image"), "{stderr}");
    assert!(stderr.contains("--strict"), "{stderr}");
    // the stand-in got no connection
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    Ok(())
}
