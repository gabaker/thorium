//! Tests that the thorctl image write commands print why image writes are gone

mod common;

use common::{TestUser, assert_success, unique};
use thorium::Error;
use thorium::models::{ImageBan, ImageBanKind, ImageBanUpdate, ImageUpdate};
use thorium::test_utilities::{self, generators};
use tokio::io::AsyncWriteExt;

/// Change the description of every exported image config under a directory
///
/// # Arguments
///
/// * `dir` - The export directory
fn change_descriptions(dir: &std::path::Path) {
    // walk the export directory
    for entry in std::fs::read_dir(dir)
        .expect("an export directory")
        .flatten()
    {
        let path = entry.path();
        // descend into nested directories
        if path.is_dir() {
            change_descriptions(&path);
            continue;
        }
        // change the description of each JSON config
        if path.extension().is_some_and(|ext| ext == "json") {
            let raw = std::fs::read_to_string(&path).expect("a config");
            let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if config.get("description").is_some() {
                config["description"] = serde_json::json!("changed by import");
                std::fs::write(&path, serde_json::to_string_pretty(&config).expect("json"))
                    .expect("the changed config");
            }
        }
    }
}

/// Run thorctl in a terminal, cancelling the re-edit prompt it shows after a failed update
///
/// # Arguments
///
/// * `user` - The user to run thorctl as
/// * `name` - The name of the run's transcript
/// * `args` - The thorctl arguments after the global flags
async fn in_terminal(user: &TestUser, name: &str, args: &[&str]) -> std::process::Output {
    // build the thorctl command line for a terminal session
    let line = format!(
        "{} --keys {} --skip-update {}",
        env!("CARGO_BIN_EXE_thorctl"),
        user.keys.display(),
        args.join(" ")
    );
    // run it under script so it gets a terminal, feeding it a down arrow and enter to cancel
    let mut child = tokio::process::Command::new("script")
        .args(["-qec", &line, "/dev/null"])
        .env("NO_PROXY", "127.0.0.1,localhost,::1")
        .env("no_proxy", "127.0.0.1,localhost,::1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start script");
    let mut stdin = child.stdin.take().expect("script's stdin");
    stdin.write_all(b"\x1b[B\r").await.expect("the keys");
    drop(stdin);
    // wait for thorctl to finish and record its transcript
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .expect("thorctl timed out")
    .expect("failed to run thorctl");
    user.record(name, args, &output);
    output
}

/// Check that a thorctl run failed printing the image writes refusal
///
/// # Arguments
///
/// * `output` - The run's output
fn assert_gone(output: &std::process::Output) {
    // the run fails and names the tool route and commands
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "the image write succeeded: {printed}"
    );
    assert!(printed.contains("/api/entities/"), "{printed}");
    assert!(printed.contains("thorctl tools"), "{printed}");
}

/// `images import`, `images edit`, and `images bans create` and `delete` fail printing that image
/// writes are gone and the image is unchanged
#[tokio::test]
async fn images_writes_print_gone() -> Result<(), Error> {
    // seed an image with a ban in the user's group, then refuse image writes there
    let user = TestUser::new("images-writes-gone").await?;
    test_utilities::faults::allow_image_writes(&user.group);
    let image = generators::images(&user.group, 1, false, &user.admin)
        .await?
        .remove(0);
    // the edit view of an external image holds no enum values, so the editor round trips it
    let plain = generators::images(&user.group, 1, true, &user.admin)
        .await?
        .remove(0);
    let ban = ImageBan::new(ImageBanKind::generic("seeded"));
    let ban_id = ban.id.to_string();
    let update = ImageUpdate::default().bans(ImageBanUpdate::default().add_ban(ban));
    user.admin
        .images
        .update(&user.group, &image.name, &update)
        .await?;
    test_utilities::faults::forbid_image_writes(&user.group);
    let before = user.admin.images.get(&user.group, &image.name).await?;
    let plain_before = user.admin.images.get(&user.group, &plain.name).await?;
    // export an image so it can be imported again
    let dir = std::env::temp_dir().join(unique("thorctl-images-"));
    let dir_str = dir.to_string_lossy().into_owned();
    let args = [
        "images",
        "export",
        "-g",
        &user.group,
        "-o",
        &dir_str,
        "--config-only",
        "--",
        &plain.name,
    ];
    assert_success(&user.run("images-export", &args).await);
    // change the exported config so importing it would update the image
    change_descriptions(&dir);
    // importing it fails
    let args = [
        "images",
        "import",
        "-g",
        &user.group,
        "-i",
        &dir_str,
        "--skip-push",
        "--overwrite",
    ];
    assert_gone(&user.run("images-import-gone", &args).await);
    // editing it fails
    let editor = dir.join("editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nsed -i 's/^description:.*/description: changed/' \"$1\"\n",
    )?;
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&editor)
        .status()?;
    let editor_str = editor.to_string_lossy().into_owned();
    let args = [
        "images",
        "edit",
        "-e",
        &editor_str,
        "--",
        &plain.name,
        &user.group,
    ];
    assert_gone(&in_terminal(&user, "images-edit-gone", &args).await);
    // creating and deleting a ban fail
    let args = [
        "images",
        "bans",
        "create",
        "--",
        &user.group,
        &image.name,
        "banned",
    ];
    assert_gone(&user.run("images-bans-create-gone", &args).await);
    let args = [
        "images",
        "bans",
        "delete",
        "--",
        &user.group,
        &image.name,
        &ban_id,
    ];
    assert_gone(&user.run("images-bans-delete-gone", &args).await);
    // the images are unchanged
    let after = user.admin.images.get(&user.group, &image.name).await?;
    assert_eq!(
        serde_json::to_value(&after)?,
        serde_json::to_value(&before)?
    );
    let plain_after = user.admin.images.get(&user.group, &plain.name).await?;
    assert_eq!(
        serde_json::to_value(&plain_after)?,
        serde_json::to_value(&plain_before)?
    );
    Ok(())
}
